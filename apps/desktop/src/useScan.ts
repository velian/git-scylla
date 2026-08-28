/**
 * Everything the window knows about discovering repositories.
 *
 * Six pieces of state that only ever move together — the rows, whether a scan
 * is running, its progress, what it could not read, which scan is being shown,
 * and whether the channel has dropped anything.
 *
 * The fold is a pure function, separate from the hook, for the reason `jobs.ts`
 * gives for the same split: what the window holds at any moment is a fold over
 * the event stream, and a fold is easier to reason about — and to test — when it
 * is not tangled with rendering. The hook is then the small part: React state,
 * one `startScan` call, and the one re-read that the fold cannot do because it
 * is I/O.
 *
 * Deliberately *not* the event listener. There is one listener in the window and
 * the drawer folds the same tick, so this exposes `apply` for that listener to
 * call rather than opening a second subscription.
 */
import { useCallback, useEffect, useState } from "react";
import { engine } from "./engine/client";
import type { DiscoveryError, RepoRow, ScanId, UiEvent } from "./bindings";

export type Progress = { found: number; probed: number };

export type ScanState = {
  repos: RepoRow[];
  scanning: boolean;
  progress: Progress | null;
  /** Everything the walk could not read, from the scan being shown. */
  errors: DiscoveryError[];
  /** How many times the channel has reported dropped events. */
  lagged: number;
  /** The scan being shown. `null` before the first one starts. */
  showing: ScanId | null;
};

export const EMPTY: ScanState = {
  repos: [],
  scanning: false,
  progress: null,
  errors: [],
  lagged: 0,
  showing: null,
};

export type Scan = ScanState & {
  /** Scan these roots, superseding whatever is running. */
  rescan: (paths: string[]) => Promise<void>;
  /** Forget every row, for when the root set changes underneath them. */
  reset: () => void;
  /** Fold one tick of events. Called from the window's single listener. */
  apply: (events: UiEvent[]) => void;
};

/**
 * Is `id` the scan the window is showing?
 *
 * The reason `ScanProgress` and `ScanDone` carry a `ScanId` at all: there are
 * two scans the moment the user adds a root while one is running, or
 * presses Refresh. Without this the older one's `ScanDone` hid the progress bar
 * and replaced the error list while the newer one was still walking — so the
 * window reported a finished scan, and the discovery errors of a scan the user
 * had already superseded.
 *
 * "Newer" is "higher": the engine allocates scan ids from a counter, so they
 * only ever increase. That is what makes this safe when the `startScan` reply
 * and that same scan's first events race each other across the IPC — whichever
 * arrives first is adopted, and the other cannot be older.
 */
export function isCurrentScan(showing: ScanId | null, id: ScanId): boolean {
  return showing === null || id >= showing;
}

/** Fold one tick of events. Returns `state` unchanged when none concerned it. */
export function fold(state: ScanState, events: UiEvent[]): ScanState {
  let next = state;
  const change = (patch: Partial<ScanState>) => {
    next = { ...next, ...patch };
  };

  for (const event of events) {
    if (event.type === "Rows") {
      change({ repos: merge(next.repos, event.value) });
      continue;
    }
    if (event.type === "BatchDone") continue;
    const inner = event.value;
    switch (inner.type) {
      case "ReposRemoved": {
        // A row for a directory that is gone is worse than no row: every action
        // offered on it will fail.
        const gone = new Set(inner.value);
        change({ repos: next.repos.filter((r) => !gone.has(r.id)) });
        break;
      }
      case "ScanProgress": {
        const { scan, found, probed } = inner.value;
        if (!isCurrentScan(next.showing, scan)) break;
        // Progress *is* evidence that a scan is running. Asserting it here
        // rather than only in `rescan` is what lets the `Lagged` arm below
        // clear the scan state instead of having to guess at it.
        change({ showing: scan, scanning: true, progress: { found, probed } });
        break;
      }
      case "ScanDone": {
        const { scan, errors } = inner.value;
        // A superseded scan finishing says nothing about the one the user is
        // waiting on, so it neither stops the progress bar nor gets to replace
        // the error list.
        if (!isCurrentScan(next.showing, scan)) break;
        change({ showing: scan, scanning: false, progress: null, errors });
        break;
      }
      case "Lagged":
        // The dropped events may have included this scan's `ScanDone`, and
        // there is no way to ask whether one is still running. Left alone, the
        // progress bar would sit there for the rest of the session claiming a
        // scan that had already finished. Cleared instead, and re-established
        // by the next `ScanProgress` if the scan is in fact still going — a bar
        // that flickers once beats one that lies.
        //
        // The rows are re-read too, by the hook: `lagged` moving is the signal.
        change({ lagged: next.lagged + 1, scanning: false, progress: null });
        break;
      default:
        break;
    }
  }
  return next;
}

export function useScan(onError: (e: unknown) => void): Scan {
  const [state, setState] = useState<ScanState>(EMPTY);

  const reset = useCallback(() => setState((s) => ({ ...s, repos: [] })), []);

  const rescan = useCallback(
    async (paths: string[]) => {
      setState((s) => ({ ...s, errors: [] }));
      if (paths.length === 0) {
        setState((s) => ({ ...s, repos: [] }));
        return;
      }
      setState((s) => ({ ...s, scanning: true }));
      try {
        // Recording the id is what lets a scan started while another is still
        // running supersede it rather than race it.
        const id = await engine.startScan(paths);
        setState((s) => (isCurrentScan(s.showing, id) ? { ...s, showing: id } : s));
      } catch (e) {
        setState((s) => ({ ...s, scanning: false }));
        onError(e);
      }
    },
    [onError],
  );

  // Stable: the only thing it closes over is a setState, so a listener holding
  // an older copy still behaves correctly.
  const apply = useCallback((events: UiEvent[]) => setState((s) => fold(s, events)), []);

  // Events were dropped, so the rows we hold may be stale. Re-read rather than
  // showing rows we can no longer vouch for. Here rather than inside the fold
  // because it is I/O, and `lagged` moving is exactly the signal for it.
  useEffect(() => {
    if (state.lagged === 0) return;
    engine
      .getSnapshot()
      .then((repos) => setState((s) => ({ ...s, repos })))
      .catch(onError);
  }, [state.lagged, onError]);

  return { ...state, rescan, reset, apply };
}

/** Upsert by id, keeping a stable order. */
function merge(prev: RepoRow[], incoming: RepoRow[]): RepoRow[] {
  const byId = new Map(prev.map((r) => [r.id, r]));
  for (const r of incoming) byId.set(r.id, r);
  return [...byId.values()].sort((a, b) => a.path.localeCompare(b.path));
}

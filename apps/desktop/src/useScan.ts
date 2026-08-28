/**
 * Discovery state: rows, scan progress, discovery errors, and which scan is
 * being shown. The fold is a pure function; the hook wraps it in `useState`
 * and exposes `apply` for the window's single event listener to call.
 */
import { useCallback, useEffect, useState } from "react";
import { engine } from "./engine/client";
import type { DiscoveryError, RepoRow, ScanId, UiEvent } from "./bindings";

export type Progress = { found: number; probed: number };

export type ScanState = {
  repos: RepoRow[];
  scanning: boolean;
  progress: Progress | null;
  /** From the scan being shown. */
  errors: DiscoveryError[];
  /** Times the channel has reported dropped events. */
  lagged: number;
  /** `null` before the first scan starts. */
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
  /** Forget every row. */
  reset: () => void;
  /** Fold one tick of events. */
  apply: (events: UiEvent[]) => void;
};

/** Whether `id` is at least as new as the scan being shown. Scan ids increase monotonically. */
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
        const gone = new Set(inner.value);
        change({ repos: next.repos.filter((r) => !gone.has(r.id)) });
        break;
      }
      case "ScanProgress": {
        const { scan, found, probed } = inner.value;
        if (!isCurrentScan(next.showing, scan)) break;
        change({ showing: scan, scanning: true, progress: { found, probed } });
        break;
      }
      case "ScanDone": {
        const { scan, errors } = inner.value;
        if (!isCurrentScan(next.showing, scan)) break;
        change({ showing: scan, scanning: false, progress: null, errors });
        break;
      }
      case "Lagged":
        // A dropped ScanDone would otherwise leave the progress bar stuck;
        // clear it and let the next ScanProgress re-establish it.
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
        const id = await engine.startScan(paths);
        setState((s) => (isCurrentScan(s.showing, id) ? { ...s, showing: id } : s));
      } catch (e) {
        setState((s) => ({ ...s, scanning: false }));
        onError(e);
      }
    },
    [onError],
  );

  const apply = useCallback((events: UiEvent[]) => setState((s) => fold(s, events)), []);

  // Dropped events may make `repos` stale; re-read rather than trust it.
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

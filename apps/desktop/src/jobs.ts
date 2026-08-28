/**
 * The drawer's state, accumulated from the event stream.
 *
 * Pure and separate from the component: what a batch looks like at any moment
 * is a fold over the events, and a fold is easier to reason about — and to fix
 * — when it is not tangled with rendering.
 *
 * Nothing here decides anything about repositories. It files events into
 * batches and rows; the states, the counts and the summary sentence all arrive
 * already decided from `crates/engine`.
 */
import type {
  Action,
  BatchId,
  BatchSummary,
  JobId,
  JobOrigin,
  JobState,
  LogLine,
  RepoId,
  UiEvent,
} from "./bindings";

export type JobRow = { id: JobId; repo: RepoId; state: JobState };

export type BatchView = {
  id: BatchId;
  origin: JobOrigin;
  /**
   * The plan headline the user confirmed, e.g. "Pull 12 repos (ff-only)".
   *
   * `null` until named. A batch's first job events are emitted *inside*
   * `start_batch`, before it has returned an id, so the drawer always meets a
   * batch through its jobs and is told what it is a moment later.
   */
  label: string | null;
  /** The template, so a failed row can be retried through a fresh plan. */
  action: Action | null;
  /**
   * When the drawer first saw it. Client-side on purpose: this drives a ticking
   * "elapsed" while the batch runs, and once it finishes the engine's own
   * duration takes over inside `line`.
   */
  firstSeen: number;
  /** In the order the engine announced them: skips first, then the plan. */
  rows: JobRow[];
  summary: BatchSummary | null;
  /** `31 ok, 3 failed, 13 skipped in 4.2s`, phrased in `core`. */
  line: string | null;
};

export type Drawer = {
  /** Newest first. Session-lived; never persisted. */
  batches: BatchView[];
  /** Transcripts by job, accumulated live. */
  logs: Record<number, LogLine[]>;
};

export const EMPTY: Drawer = { batches: [], logs: {} };

/**
 * Fold one tick's worth of events into the drawer.
 *
 * Returns the same object when nothing in the tick concerned it, so a scan
 * streaming rows does not re-render the drawer.
 */
export function apply(state: Drawer, events: UiEvent[], now: number): Drawer {
  let batches = state.batches;
  let logs = state.logs;
  let changed = false;

  /** The batch this event belongs to, created on first sighting. */
  function batchAt(id: BatchId, origin: JobOrigin): number {
    const at = batches.findIndex((b) => b.id === id);
    if (at !== -1) return at;
    batches = [
      { id, origin, label: null, action: null, firstSeen: now, rows: [], summary: null, line: null },
      ...batches,
    ];
    return 0;
  }

  for (const event of events) {
    if (event.type === "BatchDone") {
      // Not created if unknown. A batch always announces its jobs before they
      // reach a terminal state, so an unheard-of one here means its events were
      // dropped — and inventing a row-less batch with a guessed origin would be
      // worse than the silence.
      const at = batches.findIndex((b) => b.id === event.value.id);
      if (at === -1) continue;
      batches = replace(batches, at, {
        ...batches[at],
        summary: event.value.summary,
        line: event.value.line,
      });
      changed = true;
      continue;
    }
    if (event.type !== "Engine") continue;
    const inner = event.value;

    if (inner.type === "JobStateChanged") {
      const { id, batch, origin, repo, state: jobState } = inner.value;
      // A job with no batch is a lone background fetch. A drawer organised by
      // batches has nowhere to put one, so it is dropped rather than given a
      // fabricated home.
      if (batch === null) continue;
      const at = batchAt(batch, origin);
      const b = batches[at];
      const row = b.rows.findIndex((r) => r.id === id);
      const rows =
        row === -1
          ? [...b.rows, { id, repo, state: jobState }]
          : replace(b.rows, row, { id, repo, state: jobState });
      batches = replace(batches, at, { ...b, rows });
      changed = true;
      continue;
    }

    if (inner.type === "JobLogAppended") {
      const { id, lines } = inner.value;
      logs = { ...logs, [id]: [...(logs[id] ?? []), ...lines] };
      changed = true;
    }
  }

  return changed ? { batches, logs } : state;
}

/** Attach what only the caller knows: what the user confirmed. */
export function name(
  state: Drawer,
  id: BatchId,
  label: string,
  action: Action,
  now: number,
): Drawer {
  const at = state.batches.findIndex((b) => b.id === id);
  if (at === -1) {
    // The reply beat the events. Rare, but the batch must still appear.
    return {
      ...state,
      batches: [
        { id, origin: "User", label, action, firstSeen: now, rows: [], summary: null, line: null },
        ...state.batches,
      ],
    };
  }
  return { ...state, batches: replace(state.batches, at, { ...state.batches[at], label, action }) };
}

/** A transcript re-read from the engine, replacing whatever was accumulated. */
export function reload(state: Drawer, id: JobId, lines: LogLine[]): Drawer {
  return { ...state, logs: { ...state.logs, [id]: lines } };
}

/**
 * Whether a batch belongs on screen.
 *
 * The drawer shows `User` work. Background work — the fetch scheduler's — is
 * behind the toggle, **except** when it failed. A tool that fetches by itself
 * every fifteen minutes and hides the fact that it cannot is worse than one
 * that does not fetch at all.
 */
export function visible(batch: BatchView, showBackground: boolean): boolean {
  if (batch.origin === "User" || showBackground) return true;
  return batch.rows.some((r) => r.state.type === "Failed");
}

/** How far along, for the header. Skips count as done: they will not move. */
export function progress(batch: BatchView): { done: number; total: number } {
  const done = batch.rows.filter(
    (r) => r.state.type !== "Queued" && r.state.type !== "Running",
  ).length;
  return { done, total: batch.rows.length };
}

export function isRunning(batch: BatchView): boolean {
  return batch.summary === null;
}

/** `0:04`, `1:23`. */
export function elapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

/**
 * The row's state as a word.
 *
 * The one place the frontend names a `JobState`, and it is a transliteration of
 * the variant rather than an interpretation: the reason a skip carries lives in
 * `SkipReason` and is shown beside it, not folded into this.
 */
export function stateLabel(state: JobState): string {
  switch (state.type) {
    case "Queued":
      return "queued";
    case "Running":
      return "running";
    case "Ok":
      return "ok";
    case "Failed":
      return `failed (${state.value.code})`;
    case "Cancelled":
      return "cancelled";
    case "Skipped":
      return "skipped";
  }
}

function replace<T>(xs: T[], at: number, x: T): T[] {
  const out = [...xs];
  out[at] = x;
  return out;
}

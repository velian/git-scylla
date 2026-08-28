/** The drawer's state, folded from the event stream. Owns no repository logic. */
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
  /** The plan headline the user confirmed. `null` until named. */
  label: string | null;
  /** The template, so a failed row can be retried through a fresh plan. */
  action: Action | null;
  /** When the drawer first saw it. Client-side; drives a ticking "elapsed" while the batch runs. */
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

/** Fold one tick's worth of events into the drawer. */
export function apply(state: Drawer, events: UiEvent[], now: number): Drawer {
  let batches = state.batches;
  let logs = state.logs;
  let changed = false;

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

/** Whether a batch belongs on screen: user work always; background work only if it failed. */
export function visible(batch: BatchView, showBackground: boolean): boolean {
  if (batch.origin === "User" || showBackground) return true;
  return batch.rows.some((r) => r.state.type === "Failed");
}

/** How far along, for the header. Skips count as done. */
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

/** The row's state as a word, a transliteration of the `JobState` variant. */
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

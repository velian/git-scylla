/**
 * The drawer's fold.
 *
 * The design brief is one click from "40 repos done" to "what happened to
 * number 37", and everything the drawer shows is a fold over the event stream.
 * So the properties worth pinning down are the ones that decide whether that
 * click lands on the right thing: which batch a job files under, whether a row
 * replaces or duplicates, and whether a transcript accumulates in order.
 */
import { describe, expect, it } from "vitest";
import {
  apply,
  elapsed,
  EMPTY,
  isRunning,
  name,
  progress,
  reload,
  stateLabel,
  visible,
  type BatchView,
  type Drawer,
} from "./jobs";
import type { Action, JobOrigin, JobState, LogLine, UiEvent } from "./bindings";

// ---- fixtures ----------------------------------------------------------

const NOW = 1_000;
const QUEUED: JobState = { type: "Queued" };
const RUNNING: JobState = { type: "Running" };
const OK: JobState = { type: "Ok" };
const FAILED: JobState = { type: "Failed", value: { code: 1 } };
const SKIPPED: JobState = { type: "Skipped", value: { why: { type: "UpToDate" } } };

const FETCH: Action = { type: "Fetch", value: { prune: false, tags: false } };

function job(
  id: number,
  batch: number | null,
  repo: string,
  state: JobState,
  origin: JobOrigin = "User",
): UiEvent {
  return {
    type: "Engine",
    value: { type: "JobStateChanged", value: { id, batch, origin, repo, state } },
  };
}

function log(id: number, ...texts: string[]): UiEvent {
  const lines = texts.map((text) => ({ at: 0, stream: "Stdout", text })) as unknown as LogLine[];
  return { type: "Engine", value: { type: "JobLogAppended", value: { id, lines } } };
}

function batchDone(id: number, line = "1 ok in 0.1s"): UiEvent {
  return { type: "BatchDone", value: { id, summary: {}, line } } as unknown as UiEvent;
}

function only(state: Drawer): BatchView {
  expect(state.batches).toHaveLength(1);
  return state.batches[0];
}

// ---- the fold ----------------------------------------------------------

describe("apply", () => {
  it("returns the same state when nothing in the tick concerned it", () => {
    // A scan streaming a hundred rows must not re-render the drawer.
    const before = apply(EMPTY, [job(1, 1, "/a", QUEUED)], NOW);
    const after = apply(before, [
      { type: "Rows", value: [] },
      { type: "Engine", value: { type: "ScanDone", value: { scan: 1, errors: [] } } },
    ] as UiEvent[], NOW);
    expect(after).toBe(before);
  });

  it("meets a batch through its jobs, because that is the only way it can", () => {
    // A user batch emits its first job events *inside* `start_batch`, before
    // that call has returned an id, so the drawer never learns of a batch first.
    const state = apply(EMPTY, [job(1, 7, "/a", QUEUED)], NOW);
    expect(only(state)).toMatchObject({ id: 7, origin: "User", label: null, action: null });
    expect(only(state).rows).toEqual([{ id: 1, repo: "/a", state: QUEUED }]);
  });

  it("upserts a row by job id rather than appending a second one", () => {
    const state = apply(
      EMPTY,
      [job(1, 7, "/a", QUEUED), job(1, 7, "/a", RUNNING), job(1, 7, "/a", OK)],
      NOW,
    );
    expect(only(state).rows).toEqual([{ id: 1, repo: "/a", state: OK }]);
  });

  it("keeps rows in the order the engine announced them", () => {
    // Skips first, then the plan — so the drawer shows the shape of the batch
    // before anything starts running.
    const state = apply(
      EMPTY,
      [job(1, 7, "/skipped", SKIPPED), job(2, 7, "/a", QUEUED), job(3, 7, "/b", QUEUED)],
      NOW,
    );
    expect(only(state).rows.map((r) => r.repo)).toEqual(["/skipped", "/a", "/b"]);
  });

  it("keeps newer batches first", () => {
    const state = apply(EMPTY, [job(1, 1, "/a", QUEUED), job(2, 2, "/b", QUEUED)], NOW);
    expect(state.batches.map((b) => b.id)).toEqual([2, 1]);
  });

  it("drops a background job that belongs to no batch", () => {
    // A drawer organised by batches has nowhere to put a lone fetch, and a
    // fabricated home would be worse than the silence.
    expect(apply(EMPTY, [job(1, null, "/a", OK, "Background")], NOW)).toBe(EMPTY);
  });

  it("records a batch summary, and drops one for a batch it never met", () => {
    const state = apply(EMPTY, [job(1, 7, "/a", OK), batchDone(7, "1 ok in 0.1s")], NOW);
    expect(only(state)).toMatchObject({ line: "1 ok in 0.1s" });

    // Unheard-of means its events were dropped; inventing a row-less batch with
    // a guessed origin would be worse than the silence.
    expect(apply(EMPTY, [batchDone(99)], NOW)).toBe(EMPTY);
  });

  it("accumulates a transcript in order, across ticks", () => {
    const state = apply(
      apply(EMPTY, [job(1, 7, "/a", RUNNING), log(1, "one", "two")], NOW),
      [log(1, "three")],
      NOW,
    );
    expect(state.logs[1].map((l) => l.text)).toEqual(["one", "two", "three"]);
  });
});

// ---- naming, and re-reading a transcript --------------------------------

describe("name", () => {
  it("attaches what only the caller knows: what the user confirmed", () => {
    const state = name(apply(EMPTY, [job(1, 7, "/a", QUEUED)], NOW), 7, "Fetch 1 repo", FETCH, NOW);
    expect(only(state)).toMatchObject({ label: "Fetch 1 repo", action: FETCH });
    expect(only(state).rows).toHaveLength(1);
  });

  it("creates the batch when the reply beat the events", () => {
    const state = name(EMPTY, 7, "Fetch 1 repo", FETCH, NOW);
    expect(only(state)).toMatchObject({ id: 7, label: "Fetch 1 repo", rows: [] });
  });
});

describe("reload", () => {
  it("replaces an accumulated transcript rather than appending to it", () => {
    // Called only after a drop, when what was accumulated may have a hole.
    const state = reload(apply(EMPTY, [log(1, "partial")], NOW), 1, [
      { at: 0, stream: "Stdout", text: "whole" } as unknown as LogLine,
    ]);
    expect(state.logs[1].map((l) => l.text)).toEqual(["whole"]);
  });
});

// ---- what the drawer shows ---------------------------------------------

describe("visible", () => {
  const of = (origin: JobOrigin, ...states: JobState[]): BatchView =>
    only(apply(EMPTY, states.map((s, i) => job(i, 1, `/r${i}`, s, origin)), NOW));

  it("always shows user work", () => {
    expect(visible(of("User", OK), false)).toBe(true);
  });

  it("hides background work behind the toggle", () => {
    expect(visible(of("Background", OK), false)).toBe(false);
    expect(visible(of("Background", OK), true)).toBe(true);
  });

  it("shows failed background work regardless", () => {
    // A tool that fetches by itself every fifteen minutes and hides the fact
    // that it cannot is worse than one that does not fetch at all.
    expect(visible(of("Background", OK, FAILED), false)).toBe(true);
  });
});

describe("progress", () => {
  it("counts skips as done, because they will not move", () => {
    const batch = only(
      apply(
        EMPTY,
        [
          job(1, 1, "/a", SKIPPED),
          job(2, 1, "/b", OK),
          job(3, 1, "/c", RUNNING),
          job(4, 1, "/d", QUEUED),
        ],
        NOW,
      ),
    );
    expect(progress(batch)).toEqual({ done: 2, total: 4 });
  });
});

describe("isRunning", () => {
  it("is true until the summary arrives", () => {
    const state = apply(EMPTY, [job(1, 7, "/a", OK)], NOW);
    expect(isRunning(only(state))).toBe(true);
    expect(isRunning(only(apply(state, [batchDone(7)], NOW)))).toBe(false);
  });
});

describe("elapsed", () => {
  it("renders minutes and seconds", () => {
    expect(elapsed(0)).toBe("0:00");
    expect(elapsed(4_400)).toBe("0:04");
    expect(elapsed(83_000)).toBe("1:23");
    // A clock that moved backwards is not a negative duration.
    expect(elapsed(-1)).toBe("0:00");
  });
});

describe("stateLabel", () => {
  it("transliterates the variant rather than interpreting it", () => {
    // The reason a skip carries lives in `SkipReason` and is shown beside this,
    // not folded into it.
    expect(stateLabel(QUEUED)).toBe("queued");
    expect(stateLabel(RUNNING)).toBe("running");
    expect(stateLabel(OK)).toBe("ok");
    expect(stateLabel(FAILED)).toBe("failed (1)");
    expect(stateLabel({ type: "Cancelled" })).toBe("cancelled");
    expect(stateLabel(SKIPPED)).toBe("skipped");
  });
});

/** Tests the scan fold. */
import { describe, expect, it } from "vitest";
import { EMPTY, fold, isCurrentScan, type ScanState } from "./useScan";
import type { DiscoveryError, RepoRow, ScanId, UiEvent } from "./bindings";

// ---- fixtures ----------------------------------------------------------

/** Only the fields the fold touches; the rest never leaves the bridge. */
function row(path: string): RepoRow {
  return { id: path, path } as unknown as RepoRow;
}

function rows(...paths: string[]): UiEvent {
  return { type: "Rows", value: paths.map(row) };
}

function progress(scan: ScanId, found: number, probed: number): UiEvent {
  return { type: "Engine", value: { type: "ScanProgress", value: { scan, found, probed } } };
}

function done(scan: ScanId, errors: DiscoveryError[] = []): UiEvent {
  return { type: "Engine", value: { type: "ScanDone", value: { scan, errors } } };
}

const LAGGED: UiEvent = { type: "Engine", value: { type: "Lagged" } };

function removed(...ids: string[]): UiEvent {
  return { type: "Engine", value: { type: "ReposRemoved", value: ids } };
}

const UNREADABLE: DiscoveryError = {
  type: "Unreadable",
  value: { path: "/work/locked", reason: "permission denied" },
};

/** Mid-scan: scan 1 running, three of five probed. */
function scanning(): ScanState {
  return fold(EMPTY, [progress(1, 5, 3)]);
}

// ---- which scan is being shown -----------------------------------------

describe("isCurrentScan", () => {
  it("adopts anything when nothing is being shown yet", () => {
    // The `startScan` reply and that scan's first events race across IPC;
    // whichever arrives first is accepted.
    expect(isCurrentScan(null, 1)).toBe(true);
    expect(isCurrentScan(null, 9)).toBe(true);
  });

  it("accepts the same scan and newer ones, and refuses older ones", () => {
    expect(isCurrentScan(3, 3)).toBe(true);
    expect(isCurrentScan(3, 4)).toBe(true);
    expect(isCurrentScan(3, 2)).toBe(false);
  });
});

// ---- the fold ----------------------------------------------------------

describe("fold", () => {
  it("returns the same state when nothing in the tick concerned it", () => {
    const before = scanning();
    const after = fold(before, [
      { type: "BatchDone", value: { id: 1, summary: {}, line: "" } } as unknown as UiEvent,
    ]);
    expect(after).toBe(before);
  });

  it("upserts rows by id and keeps them ordered by path", () => {
    const state = fold(EMPTY, [rows("/b", "/a"), rows("/a", "/c")]);
    expect(state.repos.map((r) => r.path)).toEqual(["/a", "/b", "/c"]);
  });

  it("tracks progress and finishes on the matching ScanDone", () => {
    let state = fold(EMPTY, [progress(1, 5, 2)]);
    expect(state).toMatchObject({ scanning: true, progress: { found: 5, probed: 2 }, showing: 1 });

    state = fold(state, [done(1, [UNREADABLE])]);
    expect(state).toMatchObject({ scanning: false, progress: null, errors: [UNREADABLE] });
  });

  it("treats progress as evidence that a scan is running", () => {
    const state = fold({ ...EMPTY, showing: 1 }, [progress(1, 2, 1)]);
    expect(state.scanning).toBe(true);
  });

  it("ignores a superseded scan's progress and completion", () => {
    // Scan 2 has taken over — a root was added while scan 1 was walking.
    const state = fold(scanning(), [progress(2, 40, 4)]);
    expect(state).toMatchObject({ showing: 2, progress: { found: 40, probed: 4 } });

    const stale = fold(state, [progress(1, 5, 5), done(1, [UNREADABLE])]);
    expect(stale.scanning).toBe(true);
    expect(stale.progress).toEqual({ found: 40, probed: 4 });
    expect(stale.errors).toEqual([]);
    expect(stale.showing).toBe(2);
  });

  it("lets the newer scan finish normally after the older one is ignored", () => {
    const state = fold(scanning(), [progress(2, 40, 4), done(1), done(2, [UNREADABLE])]);
    expect(state).toMatchObject({ scanning: false, progress: null, errors: [UNREADABLE] });
  });

  it("adopts a scan first heard of through its events", () => {
    // The reply lost the race; its id is not older, so it is the current one.
    const state = fold(EMPTY, [done(7, [UNREADABLE])]);
    expect(state).toMatchObject({ showing: 7, errors: [UNREADABLE] });
  });

  it("clears the scan state when events are dropped", () => {
    const state = fold(scanning(), [LAGGED]);
    expect(state).toMatchObject({ scanning: false, progress: null, lagged: 1 });
  });

  it("lets a scan that is still running re-establish itself after a drop", () => {
    const state = fold(fold(scanning(), [LAGGED]), [progress(1, 5, 4)]);
    expect(state).toMatchObject({ scanning: true, progress: { found: 5, probed: 4 } });
    expect(state.lagged).toBe(1);
  });

  it("counts every drop, because each one may have holed a transcript", () => {
    expect(fold(EMPTY, [LAGGED, LAGGED]).lagged).toBe(2);
  });

  it("drops rows for repositories that went away", () => {
    const state = fold(EMPTY, [rows("/a", "/b", "/c"), removed("/b")]);
    expect(state.repos.map((r) => r.path)).toEqual(["/a", "/c"]);
  });

  it("takes the last word within a tick, in either direction", () => {
    expect(fold(EMPTY, [rows("/a"), removed("/a")]).repos).toEqual([]);
    expect(fold(EMPTY, [rows("/a"), removed("/a"), rows("/a")]).repos.map((r) => r.path)).toEqual([
      "/a",
    ]);
  });

  it("applies a whole tick in order", () => {
    const state = fold(EMPTY, [rows("/a"), progress(1, 2, 1), rows("/b"), done(1)]);
    expect(state.repos.map((r) => r.path)).toEqual(["/a", "/b"]);
    expect(state.scanning).toBe(false);
  });
});

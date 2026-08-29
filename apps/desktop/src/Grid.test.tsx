/**
 * @vitest-environment jsdom
 */

/** Tests the grid's click, keyboard, and context-menu handling, through rendered DOM and real event sequences. */
import { useRef, useState } from "react";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Grid, type GridHandle, type Sort } from "./Grid";
import type { RepoId, RepoRow } from "./bindings";

// The real client reaches for Tauri internals that do not exist under jsdom.
vi.mock("./engine/client", () => ({
  engine: {
    handOff: vi.fn().mockResolvedValue(undefined),
    refreshRepo: vi.fn().mockResolvedValue(undefined),
    fetchNow: vi.fn().mockResolvedValue(undefined),
  },
}));

function repo(name: string): RepoRow {
  return {
    badge: "Clean",
    badge_label: "clean",
    badge_rank: 7,
    stale: false,
    status: "",
    fetch_cell: { status: { type: "NoRemote" }, problem: false, detail: null },
    id: `/work/${name}` as RepoId,
    path: `/work/${name}`,
    kind: { type: "Normal" },
    head: { type: "Branch", value: "main" },
    head_oid: null,
    upstream: null,
    remotes: [],
    work: { staged: 0, modified: 0, untracked: 0, conflicted: 0 },
    op: null,
    stashes: 0,
    fetch: { last_attempt: null, last_success: null, schedule: { type: "Disabled" } },
    probed_at: 0,
    outcome: { type: "Ok" },
    from_cache: false,
  };
}

const ROWS = ["alpha", "bravo", "charlie", "delta"].map(repo);

function Harness({ rows = ROWS }: { rows?: RepoRow[] }) {
  const [selected, setSelected] = useState<Set<RepoId>>(new Set());
  const [sort, setSort] = useState<Sort>({ key: "name", dir: "asc" });
  return (
    <div>
      <button>outside</button>
      <Grid
        rows={rows}
        roots={["/work"]}
        selected={selected}
        onSelected={setSelected}
        onError={() => {}}
        sort={sort}
        onSort={setSort}
      />
    </div>
  );
}

function FilterAndGrid({ rows = ROWS }: { rows?: RepoRow[] }) {
  const [selected, setSelected] = useState<Set<RepoId>>(new Set());
  const [sort, setSort] = useState<Sort>({ key: "name", dir: "asc" });
  const gridRef = useRef<GridHandle>(null);
  return (
    <div>
      <input
        aria-label="filter"
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "Enter") {
            e.preventDefault();
            gridRef.current?.focusFirst();
          }
        }}
      />
      <Grid
        ref={gridRef}
        rows={rows}
        roots={["/work"]}
        selected={selected}
        onSelected={setSelected}
        onError={() => {}}
        sort={sort}
        onSort={setSort}
      />
    </div>
  );
}

let scrolled: string[];

beforeEach(() => {
  scrolled = [];
  Element.prototype.scrollIntoView = vi.fn(function (this: Element) {
    const row = this.closest("tr");
    const cell = row?.querySelector(".col-name");
    scrolled.push(cell?.textContent ?? "?");
  });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const rowFor = (name: string) =>
  screen.getAllByRole("row").find((r) => r.querySelector(".col-name")?.textContent === name)!;
const selectedNames = () =>
  screen
    .getAllByRole("row")
    .filter((r) => r.getAttribute("aria-selected") === "true")
    .map((r) => r.querySelector(".col-name")?.textContent);

describe("selecting rows", () => {
  it("adds to the selection when a row's checkbox is ticked", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(within(rowFor("alpha")).getByRole("checkbox"));
    expect(selectedNames()).toEqual(["alpha"]);

    await user.click(within(rowFor("charlie")).getByRole("checkbox"));
    expect(selectedNames()).toEqual(["alpha", "charlie"]);

    await user.click(within(rowFor("alpha")).getByRole("checkbox"));
    expect(selectedNames()).toEqual(["charlie"]);
  });

  it("replaces the selection when the row itself is clicked", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(rowFor("alpha"));
    expect(selectedNames()).toEqual(["alpha"]);

    await user.click(rowFor("charlie"));
    expect(selectedNames()).toEqual(["charlie"]);
  });
});

describe("the keyboard cursor", () => {
  it("does not travel through the first row on the first click", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(rowFor("charlie"));

    expect(scrolled).toEqual(["charlie"]);
    expect(scrolled).not.toContain("alpha");
  });

  it("does not let the browser scroll the grid to the top on a click", async () => {

    const user = userEvent.setup();
    const focus = vi.spyOn(HTMLElement.prototype, "focus");
    render(<Harness />);

    await user.click(rowFor("charlie"));

    const grid = screen.getByRole("grid");
    const calls = focus.mock.calls.filter((_, i) => focus.mock.instances[i] === grid);
    expect(calls.length).toBeGreaterThan(0);
    for (const [options] of calls) expect(options).toEqual({ preventScroll: true });
  });

  it("still starts at the first row when the grid is reached by Tab", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.tab();
    await user.tab();

    expect(rowFor("alpha").className).toContain("is-cursor");
  });
});

describe("handing off from the filter box", () => {
  it("focuses the first row on ArrowDown", async () => {
    const user = userEvent.setup();
    render(<FilterAndGrid />);

    await user.click(screen.getByLabelText("filter"));
    await user.keyboard("{ArrowDown}");

    expect(rowFor("alpha").className).toContain("is-cursor");
    expect(document.activeElement?.getAttribute("role")).toEqual("grid");
  });

  it("leaves the filter box on screen when handing off", async () => {
    const user = userEvent.setup();
    const focus = vi.spyOn(HTMLElement.prototype, "focus");
    render(<FilterAndGrid />);

    await user.click(screen.getByLabelText("filter"));
    await user.keyboard("{ArrowDown}");

    const grid = screen.getByRole("grid");
    const calls = focus.mock.calls.filter((_, i) => focus.mock.instances[i] === grid);
    expect(calls.length).toBeGreaterThan(0);
    for (const [options] of calls) expect(options).toEqual({ preventScroll: true });
    expect(scrolled).toEqual(["alpha"]);
  });

  it("focuses the first row on Enter", async () => {
    const user = userEvent.setup();
    render(<FilterAndGrid />);

    await user.click(screen.getByLabelText("filter"));
    await user.keyboard("{Enter}");

    expect(rowFor("alpha").className).toContain("is-cursor");
    expect(document.activeElement?.getAttribute("role")).toEqual("grid");
  });

  it("does nothing when there are no rows to land on", async () => {
    const user = userEvent.setup();
    render(<FilterAndGrid rows={[]} />);

    await user.click(screen.getByLabelText("filter"));
    await user.keyboard("{ArrowDown}");

    expect(document.activeElement?.getAttribute("aria-label")).toEqual("filter");
  });
});

describe("the row context menu", () => {
  const openMenu = async (user: ReturnType<typeof userEvent.setup>, name: string) => {
    await user.pointer({ target: rowFor(name), keys: "[MouseRight]" });
    expect(screen.getByRole("button", { name: "Reveal in Finder" })).toBeDefined();
  };

  it("closes on a click outside the grid", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await openMenu(user, "bravo");

    await user.click(screen.getByRole("button", { name: "outside" }));

    expect(screen.queryByRole("button", { name: "Reveal in Finder" })).toBeNull();
  });

  it("closes on Escape", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await openMenu(user, "bravo");

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("button", { name: "Reveal in Finder" })).toBeNull();
  });

  it("stays open for a click on itself", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await openMenu(user, "bravo");

    await user.click(screen.getByRole("list"));

    expect(screen.getByRole("button", { name: "Reveal in Finder" })).toBeDefined();
  });
});

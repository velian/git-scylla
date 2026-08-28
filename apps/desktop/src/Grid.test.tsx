/**
 * @vitest-environment jsdom
 */

/**
 * The grid's click handling.
 *
 * Three bugs lived here at once, and none of them was visible in the handler
 * that caused it: a context menu that outlived its dismissal and covered the
 * rows beneath, a checkbox wired to nothing, and a first click that scrolled the
 * list out from under the pointer. All three are about what a click *reaches*,
 * which is why they are tested through rendered DOM and real event sequences
 * rather than by calling the handlers.
 *
 * jsdom does no layout, so scroll positions cannot be asserted here. The cause
 * can: `scrollIntoView` is stubbed and the rows it is called on are recorded,
 * which is the thing that was wrong.
 */
import { useState } from "react";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Grid, type Sort } from "./Grid";
import type { RepoId, RepoRow } from "./bindings";

// The row menu hands off and re-probes through this. Nothing here exercises
// those, and the real client reaches for Tauri internals that do not exist
// under jsdom.
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
    fetch_cell: { text: "no remote", problem: false, detail: null },
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

/** The selection state the real window owns, so the grid can be driven. */
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

/** The rows `scrollIntoView` was called on, in order, by repository name. */
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
  // Explicit: testing-library only registers its own cleanup when the test
  // framework's globals are injected, and vitest is not configured to.
  // Without this every render stacks up in one body and queries match twice.
  cleanup();
  vi.clearAllMocks();
});

// By the name column specifically: with `/work` as the root, the path column
// renders the same word, so a bare text query matches two cells.
const rowFor = (name: string) =>
  screen.getAllByRole("row").find((r) => r.querySelector(".col-name")?.textContent === name)!;
const selectedNames = () =>
  screen
    .getAllByRole("row")
    .filter((r) => r.getAttribute("aria-selected") === "true")
    .map((r) => r.querySelector(".col-name")?.textContent);

describe("selecting rows", () => {
  it("adds to the selection when a row's checkbox is ticked", async () => {
    // The checkbox was inert: `onChange` did nothing and `onClick` stopped the
    // row handler, so neither acted. Ticking one must add exactly one
    // repository and leave the rest of the selection alone.
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(within(rowFor("alpha")).getByRole("checkbox"));
    expect(selectedNames()).toEqual(["alpha"]);

    await user.click(within(rowFor("charlie")).getByRole("checkbox"));
    expect(selectedNames()).toEqual(["alpha", "charlie"]);

    // And untick, since a checkbox that only adds is half a control.
    await user.click(within(rowFor("alpha")).getByRole("checkbox"));
    expect(selectedNames()).toEqual(["charlie"]);
  });

  it("replaces the selection when the row itself is clicked", async () => {
    // The contrast the checkbox exists for. A plain click is "just this one",
    // and making the box toggle must not have made the row toggle too.
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
    // Focusing the grid defaults the cursor to the first row so Tab has
    // somewhere to start. A click focuses it too, and then moves the cursor to
    // the row it landed on — so the cursor briefly sat on row one, and the
    // `scrollIntoView` that keeps it visible dragged the list to the top and
    // back. Only ever the first click, because afterwards the cursor is set.
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(rowFor("charlie"));

    expect(scrolled).toEqual(["charlie"]);
    expect(scrolled).not.toContain("alpha");
  });

  it("still starts at the first row when the grid is reached by Tab", async () => {
    // The other half: the default is what makes keyboard focus visible, and
    // skipping it for pointer focus must not have removed it for Tab.
    const user = userEvent.setup();
    render(<Harness />);

    await user.tab();
    await user.tab();

    expect(rowFor("alpha").className).toContain("is-cursor");
  });
});

describe("the row context menu", () => {
  const openMenu = async (user: ReturnType<typeof userEvent.setup>, name: string) => {
    await user.pointer({ target: rowFor(name), keys: "[MouseRight]" });
    expect(screen.getByRole("button", { name: "Reveal in Finder" })).toBeDefined();
  };

  it("closes on a click outside the grid", async () => {
    // It is `position: fixed` and opaque, and its own click handler stops
    // propagation — so a menu left open sits over the rows beneath it and eats
    // their clicks. Every way of stranding it is a click somewhere the grid
    // cannot see: the toolbar, the sidebar, Clear.
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
    // The dismissal must not be so eager that the menu cannot be used.
    const user = userEvent.setup();
    render(<Harness />);
    await openMenu(user, "bravo");

    await user.click(screen.getByRole("list"));

    expect(screen.getByRole("button", { name: "Reveal in Finder" })).toBeDefined();
  });
});

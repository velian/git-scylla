import { useEffect, useMemo, useRef, useState } from "react";
import type { RepoId, RepoRow } from "./bindings";
import {
  branch,
  compare,
  name,
  outcomeDetail,
  relativePath,
  type SortDir,
  type SortKey,
} from "./columns";
import { engine } from "./engine/client";
import { useDismiss } from "./useDismiss";

const COLUMNS: { key: SortKey; label: string; className: string }[] = [
  { key: "name", label: "Name", className: "col-name" },
  { key: "path", label: "Path", className: "col-path" },
  { key: "branch", label: "Branch", className: "col-branch" },
  { key: "badge", label: "State", className: "col-badge" },
  { key: "status", label: "Status", className: "col-status" },
  { key: "fetch", label: "Fetch", className: "col-fetch" },
];

export type Sort = { key: SortKey; dir: SortDir };

type Props = {
  rows: RepoRow[];
  roots: string[];
  selected: Set<RepoId>;
  onSelected: (next: Set<RepoId>) => void;
  onError: (e: unknown) => void;
  /** Lifted, because View ▸ Sort By sets it from the menu bar. */
  sort: Sort;
  onSort: (next: Sort) => void;
};

/**
 * The repository grid.
 *
 * No virtualization: under a hundred rows it buys nothing.
 */
export function Grid({ rows, roots, selected, onSelected, onError, sort, onSort }: Props) {
  const [anchor, setAnchor] = useState<RepoId | null>(null);
  const [menu, setMenu] = useState<{ row: RepoRow; x: number; y: number } | null>(null);
  // The keyboard's idea of "here". Separate from the selection, the way a Mac
  // list works: ⇧↓ extends from the anchor, Space toggles what is under the
  // cursor without disturbing anything else.
  const [cursor, setCursor] = useState<RepoId | null>(null);
  const cursorRow = useRef<HTMLTableRowElement>(null);
  const container = useRef<HTMLDivElement>(null);
  // Set only for the focus a click causes. See `onFocus` below.
  const fromPointer = useRef(false);

  const sorted = useMemo(
    () => [...rows].sort((a, b) => compare(a, b, sort.key, sort.dir)),
    [rows, sort],
  );

  // Keep the cursor on screen when the keyboard moves it past the fold.
  useEffect(() => {
    cursorRow.current?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  function toggleSort(key: SortKey) {
    onSort(sort.key === key ? { key, dir: sort.dir === "asc" ? "desc" : "asc" } : { key, dir: "asc" });
  }

  /**
   * Arrow keys and Space.
   *
   * On the container rather than the document, so the filter box keeps its own
   * arrow keys and the drawer's transcript keeps its scrolling. Clicking a row
   * focuses the container, because nothing inside a `<td>` takes focus itself.
   */
  function onKeyDown(e: React.KeyboardEvent) {
    if (sorted.length === 0) return;
    const here = cursor === null ? -1 : sorted.findIndex((r) => r.id === cursor);

    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const step = e.key === "ArrowDown" ? 1 : -1;
      // From nowhere, the first press lands on an end rather than the middle.
      const to = here === -1 ? (step === 1 ? 0 : sorted.length - 1) : clamp(here + step, sorted.length);
      const row = sorted[to];
      setCursor(row.id);
      if (e.shiftKey && anchor !== null) {
        onSelected(extendedTo(to));
      } else {
        setAnchor(row.id);
        onSelected(new Set([row.id]));
      }
      return;
    }

    if (e.key === " ") {
      e.preventDefault();
      if (here === -1) return;
      toggle(sorted[here].id);
    }
  }

  /**
   * Add or remove one repository, leaving the rest of the selection alone.
   *
   * What the checkbox and Space both mean — as against a plain click, which
   * replaces the selection with the row it landed on. Shared so the two cannot
   * disagree about what ticking a box does.
   */
  /**
   * The selection extended from the anchor to `to`.
   *
   * Over what is *displayed*, which is what the user sees selected — and shared
   * between ⇧-click and ⇧-arrow, which are the same gesture reached two ways.
   */
  function extendedTo(to: number): Set<RepoId> {
    const next = new Set(selected);
    const from = anchor === null ? -1 : sorted.findIndex((r) => r.id === anchor);
    if (from !== -1) {
      const [lo, hi] = from < to ? [from, to] : [to, from];
      for (let i = lo; i <= hi; i++) next.add(sorted[i].id);
    }
    return next;
  }

  function toggle(id: RepoId) {
    const next = new Set(selected);
    next.has(id) ? next.delete(id) : next.add(id);
    setAnchor(id);
    setCursor(id);
    onSelected(next);
  }

  function click(event: React.MouseEvent, row: RepoRow, index: number) {
    // Extending leaves the anchor where it is; that is what makes a second
    // ⇧-click re-range from the same place rather than from the last one.
    if (event.shiftKey && anchor !== null) {
      setCursor(row.id);
      onSelected(extendedTo(index));
      return;
    }
    const next = new Set(selected);
    if (event.metaKey || event.ctrlKey) {
      next.has(row.id) ? next.delete(row.id) : next.add(row.id);
    } else {
      next.clear();
      next.add(row.id);
    }
    setAnchor(row.id);
    setCursor(row.id);
    onSelected(next);
  }

  const allSelected = sorted.length > 0 && sorted.every((r) => selected.has(r.id));

  return (
    <div
      className="grid"
      ref={container}
      tabIndex={0}
      role="grid"
      aria-label="Repositories"
      onKeyDown={onKeyDown}
      // Reaching the grid by Tab has to show where the arrow keys will start.
      // Without this the outline lives on a cursor that does not exist yet, and
      // focus is invisible.
      //
      // By Tab only. A click focuses the grid as well, and is about to put the
      // cursor on the row it landed on — defaulting to the first row in between
      // scrolls the list to the top and straight back down again.
      onFocus={() => {
        if (fromPointer.current) return;
        if (cursor === null && sorted.length > 0) setCursor(sorted[0].id);
      }}
      // Focused explicitly rather than relying on a click landing on a
      // `tabindex` ancestor: that behaviour differs between engines, and this
      // ships on WKWebView while it is developed against Chromium. Without
      // focus the arrow keys go nowhere.
      //
      // `focus()` dispatches synchronously, so the flag spans exactly the focus
      // this causes and nothing else.
      onMouseDown={() => {
        fromPointer.current = true;
        container.current?.focus();
        fromPointer.current = false;
      }}
      onClick={() => setMenu(null)}
    >
      <table>
        <thead>
          <tr>
            <th className="col-check">
              <input
                type="checkbox"
                checked={allSelected}
                aria-label="Select all"
                onChange={() =>
                  onSelected(allSelected ? new Set() : new Set(sorted.map((r) => r.id)))
                }
              />
            </th>
            {COLUMNS.map((c) => (
              <th key={c.key} className={c.className}>
                <button onClick={() => toggleSort(c.key)}>
                  {c.label}
                  {sort.key === c.key && <span className="caret">{sort.dir === "asc" ? "▲" : "▼"}</span>}
                </button>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {sorted.map((row, index) => {
            const problem = outcomeDetail(row);
            // Both phrased in Rust — see `row.rs`. The grid places them.
            const fetch = row.fetch_cell;
            return (
              <tr
                key={row.id}
                ref={row.id === cursor ? cursorRow : undefined}
                aria-selected={selected.has(row.id)}
                className={`${selected.has(row.id) ? "is-selected" : ""} ${problem ? "is-untrusted" : ""} ${row.stale ? "is-stale" : ""} ${row.id === cursor ? "is-cursor" : ""}`}
                onClick={(e) => click(e, row, index)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setMenu({ row, x: e.clientX, y: e.clientY });
                }}
              >
                <td className="col-check">
                  <input
                    type="checkbox"
                    checked={selected.has(row.id)}
                    aria-label={`Select ${name(row)}`}
                    // Toggled from the click, not from `onChange`, because this
                    // is also where the row's own handler is stopped — a plain
                    // click replaces the selection, and ticking a box must not.
                    // Whether `onChange` still fires after that stop is a detail
                    // of React's event plugins; the click is the event actually
                    // being handled, so it does the work. `readOnly` marks the
                    // box as driven by `checked` rather than by the DOM.
                    readOnly
                    onClick={(e) => {
                      e.stopPropagation();
                      toggle(row.id);
                    }}
                  />
                </td>
                <td className="col-name">
                  {name(row)}
                  {/* Said, not merely implied by the dimming: a row that looks
                      faint could as easily be a rendering quirk, and the point
                      of the cache is that the user can tell what has been
                      verified this session from what has not. */}
                  {row.stale && (
                    <span className="stale-mark" title="Not yet re-read this session">
                      refreshing
                    </span>
                  )}
                </td>
                <td className="col-path" title={row.path}>
                  {relativePath(row, roots)}
                </td>
                <td className="col-branch">{branch(row)}</td>
                <td className="col-badge">
                  <span
                    className={`badge badge--${row.badge_label}`}
                    title={problem}
                  >
                    {row.badge_label}
                  </span>
                </td>
                <td className="col-status">{row.status}</td>
                <td className={`col-fetch ${fetch.problem ? "is-problem" : ""}`} title={fetch.detail ?? undefined}>
                  {fetch.text}
                  {fetch.problem && (
                    <button
                      className="inline-action"
                      onClick={(e) => {
                        e.stopPropagation();
                        engine.fetchNow(row.id).catch(onError);
                      }}
                    >
                      Fetch now
                    </button>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {menu && <RowMenu {...menu} onClose={() => setMenu(null)} onError={onError} />}
    </div>
  );
}

function RowMenu({
  row,
  x,
  y,
  onClose,
  onError,
}: {
  row: RepoRow;
  x: number;
  y: number;
  onClose: () => void;
  onError: (e: unknown) => void;
}) {
  const run = (p: Promise<unknown>) => {
    p.catch(onError).finally(onClose);
  };

  const root = useRef<HTMLUListElement>(null);
  useDismiss(root, onClose);

  return (
    <ul ref={root} className="menu" style={{ left: x, top: y }} onClick={(e) => e.stopPropagation()}>
      <li>
        <button onClick={() => run(engine.handOff("Finder", row.path))}>Reveal in Finder</button>
      </li>
      <li>
        <button onClick={() => run(engine.handOff("Terminal", row.path))}>Open in Terminal</button>
      </li>
      <li>
        <button onClick={() => run(engine.handOff("Editor", row.path))}>Open in editor</button>
      </li>
      <li>
        <button onClick={() => run(navigator.clipboard.writeText(row.path))}>Copy path</button>
      </li>
      <li>
        <button onClick={() => run(engine.refreshRepo(row.id))}>Refresh</button>
      </li>
    </ul>
  );
}

function clamp(i: number, len: number): number {
  return Math.max(0, Math.min(len - 1, i));
}

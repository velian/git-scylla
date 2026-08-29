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
import { FetchCellView } from "./FetchCell";
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
  sort: Sort;
  onSort: (next: Sort) => void;
};

/** The repository grid. No virtualization: under a hundred rows it buys nothing. */
export function Grid({ rows, roots, selected, onSelected, onError, sort, onSort }: Props) {
  const [anchor, setAnchor] = useState<RepoId | null>(null);
  const [menu, setMenu] = useState<{ row: RepoRow; x: number; y: number } | null>(null);
  const [cursor, setCursor] = useState<RepoId | null>(null);
  const cursorRow = useRef<HTMLTableRowElement>(null);
  const container = useRef<HTMLDivElement>(null);
  const fromPointer = useRef(false);

  const sorted = useMemo(
    () => [...rows].sort((a, b) => compare(a, b, sort.key, sort.dir)),
    [rows, sort],
  );

  useEffect(() => {
    cursorRow.current?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  function toggleSort(key: SortKey) {
    onSort(sort.key === key ? { key, dir: sort.dir === "asc" ? "desc" : "asc" } : { key, dir: "asc" });
  }

  function onKeyDown(e: React.KeyboardEvent) {
    if (sorted.length === 0) return;
    const here = cursor === null ? -1 : sorted.findIndex((r) => r.id === cursor);

    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const step = e.key === "ArrowDown" ? 1 : -1;
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
      // Only a Tab-focus should default the cursor to the first row; a
      // pointer focus is about to place it on the row that was clicked.
      onFocus={() => {
        if (fromPointer.current) return;
        if (cursor === null && sorted.length > 0) setCursor(sorted[0].id);
      }}
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
                    readOnly
                    onClick={(e) => {
                      e.stopPropagation();
                      toggle(row.id);
                    }}
                  />
                </td>
                <td className="col-name">
                  {name(row)}
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
                <FetchCellView id={row.id} cell={row.fetch_cell} onError={onError} />
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

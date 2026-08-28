/**
 * Column ordering for the repository grid, and the two columns derived from a
 * path.
 *
 * Deliberately thin. Anything that decides what a repository *is* — its badge,
 * whether an action may run on it, whether an expression matches — happens in
 * Rust and arrives here already decided, and so does anything that *phrases*
 * one: the status column and the fetch cell are projected by `row.rs` as
 * `status` and `fetch_cell`, because the CLI renders the same two and a second
 * implementation of either is a second thing to be wrong.
 */
import type { RepoRow } from "./bindings";

export type SortKey = "badge" | "name" | "path" | "branch" | "status" | "fetch";
export type SortDir = "asc" | "desc";

export function name(row: RepoRow): string {
  return basename(row.path);
}

export function basename(path: string): string {
  return path.split("/").filter(Boolean).pop() ?? path;
}

export function relativePath(row: RepoRow, roots: string[]): string {
  return relativeTo(row.path, roots);
}

/**
 * Path relative to the longest root that contains it, else the whole path.
 *
 * Takes a path rather than a row because the plan sheet has only
 * [`RepoId`]s — and a `RepoId` *is* the canonicalized path, so nothing is
 * lost. Everything else about a repository still needs the row.
 */
export function relativeTo(path: string, roots: string[]): string {
  let best = "";
  for (const root of roots) {
    const base = root.endsWith("/") ? root : root + "/";
    if (path.startsWith(base) && root.length > best.length) best = root;
  }
  if (best === "") return path;
  const rest = path.slice(best.length).replace(/^\//, "");
  return rest === "" ? basename(path) : rest;
}

export function branch(row: RepoRow): string {
  switch (row.head.type) {
    case "Branch":
      return row.head.value;
    case "Unborn":
      return `${row.head.value} (unborn)`;
    case "Detached":
      return `(${row.head.value.slice(0, 7)})`;
  }
}

/** The tooltip for a row whose probe did not succeed. */
export function outcomeDetail(row: RepoRow): string | undefined {
  switch (row.outcome.type) {
    case "Ok":
      return undefined;
    case "Timeout":
      return "The probe timed out. This repository may be on a slow or unmounted volume.";
    case "Error":
      return row.outcome.value;
  }
}

export function compare(a: RepoRow, b: RepoRow, key: SortKey, dir: SortDir): number {
  const sign = dir === "asc" ? 1 : -1;
  const byPath = a.path.localeCompare(b.path);
  switch (key) {
    // `badgeRank` comes from Rust because the ordering is the declaration order
    // of the Badge enum, which a TypeScript string union cannot express.
    case "badge":
      return sign * (a.badge_rank - b.badge_rank || byPath);
    case "name":
      return sign * (name(a).localeCompare(name(b)) || byPath);
    case "path":
      return sign * byPath;
    case "branch":
      return sign * (branch(a).localeCompare(branch(b)) || byPath);
    case "status":
      return sign * (weight(b) - weight(a) || byPath);
    case "fetch":
      return sign * ((lastFetch(a) ?? 0) - (lastFetch(b) ?? 0) || byPath);
  }
}

/** How much is going on in a repository, for the status column's sort. */
function weight(row: RepoRow): number {
  const w = row.work;
  const sync = row.upstream?.sync;
  return (
    w.conflicted * 1000 +
    (sync?.ahead ?? 0) * 100 +
    (sync?.behind ?? 0) * 100 +
    w.modified * 10 +
    w.staged * 10 +
    w.untracked
  );
}

function lastFetch(row: RepoRow): number | null {
  return row.upstream?.last_fetch ?? row.fetch.last_success ?? null;
}

/** Grouping repositories under the roots they came from, for the sidebar. */
import type { DiscoveryError, RepoSnapshot } from "./bindings";
import { basename } from "./columns";

export type RootSummary = {
  path: string;
  name: string;
  count: number;
  /** Places under this root the walk could not read. */
  unreadable: number;
  /** True when the root yielded nothing *and* something refused to be read. */
  looksBlocked: boolean;
};

/** Is `path` inside `root`, by path component rather than by string prefix? */
export function isUnder(path: string, root: string): boolean {
  if (path === root) return true;
  const base = root.endsWith("/") ? root : root + "/";
  return path.startsWith(base);
}

/** Count repositories per root. Each is attributed to its longest matching root. */
export function summarise(
  roots: string[],
  repos: RepoSnapshot[],
  errors: DiscoveryError[],
): RootSummary[] {
  const counts = new Map(roots.map((r) => [r, 0]));
  for (const repo of repos) {
    let best: string | null = null;
    for (const root of roots) {
      if (isUnder(repo.path, root) && (best === null || root.length > best.length)) {
        best = root;
      }
    }
    if (best !== null) counts.set(best, (counts.get(best) ?? 0) + 1);
  }

  return roots.map((path) => {
    const unreadable = countUnreadable(errors, path);
    const count = counts.get(path) ?? 0;
    return {
      path,
      name: basename(path),
      count,
      unreadable,
      looksBlocked: count === 0 && unreadable > 0,
    };
  });
}

function countUnreadable(errors: DiscoveryError[], root: string): number {
  let n = 0;
  for (const e of errors) {
    switch (e.type) {
      case "Unreadable":
        if (isUnder(e.value.path, root)) n += 1;
        break;
      case "UnusableRoot":
        if (e.value === root) n += 1;
        break;
      case "MoreUnreadable":
        n += e.value;
        break;
    }
  }
  return n;
}

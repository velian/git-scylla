# testkit

`testkit` builds real git repositories on disk and pairs each one with the
snapshot it must produce. Downstream crates run their real code against the
set and assert the output matches. There is no mocked git anywhere in this
pipeline — every fixture is produced by invoking the real `git` binary.

Three crates consume it: `discovery` (does the walker find the right paths),
`probe` (does the parser produce the right `RepoSnapshot`), and `engine` (does
planning produce the right actions over the right repositories).

## Layout on disk

`FixtureSet::build(dir)` populates `dir` like this:

```
<dir>/
  home/       scratch HOME; no fixture reads the developer's ~/.gitconfig
  scratch/    seed clones used to advance an origin after cloning from it
  origins/    bare repositories acting as remotes
  repos/      the scan root — every fixture a walker should discover
```

`origins/` and `scratch/` sit outside `repos/`, so they are never themselves
discovered by a scan. `FixtureSet::scan_root` points at `repos/`.

## Components

```
              Git ──────── hermetic `git` invocation, pinned env and config
               │
               ▼
           Builder ──────── assembles fixtures in stages, writes to `repos/`
               │
               ▼
        FixtureSet ──────── { dir, scan_root, fixtures: Vec<Fixture> }
               │
               ▼
          Fixture ──────── { name, path, expect: Expect, nested_only }
```

`Git` runs `git` with a fixed author/committer identity and date, an isolated
`HOME`, and every global/system config source disabled. Two fixture runs
produce byte-identical repositories, on any machine.

`Builder` is private. It exposes no fixture-authoring API beyond `build()`;
every fixture is added by name inside `set.rs`.

`Fixture.nested_only` marks a fixture that only a scan with `--nested`
should find — a repository living inside another repository's worktree, or a
submodule.

## Build pipeline

`FixtureSet::build` runs `Builder` through five stages, in order:

```
shapes ─▶ worktree_and_submodule ─▶ upstreams ─▶ worktree_states ─▶ in_progress
```

Each stage appends `Fixture` entries via `Builder::push` /
`Builder::push_nested`. `shapes` runs first: every later stage's expectation
is stated as a delta from the `clean` fixture it defines.

- **shapes** — repository kinds: clean, unborn, bare, bare with packed refs,
  a repository nested inside another's worktree.
- **worktree_and_submodule** — the two `.git`-is-a-file cases: a linked
  worktree and a submodule.
- **upstreams** — HEAD and upstream tracking: in sync, no upstream, ahead,
  behind, diverged, upstream gone, and worktree-dirty combinations of those.
- **worktree_states** — untracked, modified, staged, staged-and-modified,
  renamed, adversarial filenames, stashed.
- **in_progress** — half-finished operations: conflicted merge, merge stopped
  without conflict, rebase, cherry-pick, revert, bisect. Every one is reached
  by running a git command that fails or is told to stop, not by hand-writing
  `.git/MERGE_HEAD` and friends.

## Expectation and comparison

`Expect` is a hand-written prediction of a `RepoSnapshot`, not the type
itself. Three fields are excluded because no generator can predict them:
`probed_at`, the mtime behind `last_fetch`, and the clock inside
`FetchSchedule::Due`. Every other field is asserted exactly.

`Expect::to_json` and `normalize` project their respective inputs — a
declared `Expect`, a real `RepoSnapshot` — into the same JSON shape:

```
   Expect::to_json(name)                    normalize(name, snapshot)
          │                                          │
          └──────────────► JSON ◄───────────────────┘
                             │
                        assert_eq!
```

A consuming test builds the set, probes every discovered path, normalizes
each snapshot, and diffs it against the fixture's declared expectation. A
mismatch is a JSON diff between two values of identical shape, not a
hand-rolled field comparison.

## `gen` binary

`cargo run -p git-scylla-testkit --bin gen -- <dir>` builds the set into
`<dir>` and prints every fixture's name and path. Use it to inspect the
repositories a fixture produces before writing its `Expect`, or to poke at
one manually with `git`.

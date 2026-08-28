# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

This repo is **single-context**: one `CONTEXT.md` and one `docs/adr/` at the root, shared by every crate in the Cargo workspace.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root: the glossary of domain terms.
- **`docs/adr/`**: read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

## File structure

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-fsevents-for-tree-watching.md
│   └── 0002-sqlite-for-the-repo-store.md
├── crates/
│   ├── core/
│   ├── discovery/
│   ├── engine/
│   ├── exec/
│   ├── probe/
│   ├── store/
│   ├── watch/
│   └── testkit/
└── apps/
    ├── cli/
    └── desktop/
```

The workspace has many crates, but they share one domain vocabulary, so terms live in a single root `CONTEXT.md` rather than one per crate. If a crate's language ever genuinely diverges, split it out: add a root `CONTEXT-MAP.md` pointing at per-crate `CONTEXT.md` files, with crate-scoped `crates/<name>/docs/adr/` for decisions local to that crate.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (event-sourced orders), but worth reopening because…_

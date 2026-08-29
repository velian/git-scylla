# git-scylla


## What is this

A CLI and a Tauri desktop app that treat a folder of git repositories as one
working set: scan them all, see what's dirty or behind, then fetch, pull,
push, stash, commit, branch, tag, sync, or run an arbitrary `git` command
across whichever of them match a selection expression (e.g. `dirty & branch:main`).

## What does it do

- `scan` — walk one or more roots and report every repository's state (branch, dirty, ahead/behind)
- `fetch` / `pull` / `push` — batch the usual remote operations, with a `--daemon` mode that fetches on its own schedule
- `stash` / `stash-pop` / `checkout` / `branch` / `commit` — batch the usual local operations, with `{repo}`/`{branch}`/`{date}` templating in messages and refs
- `sync-default` — stash, hop to the default branch, pull, hop back, and pop, as one unit
- `dev-tag` — cut the next tag in a pre-release series, resolved per repository from its own tags
- `run -- <args>` — pass any git subcommand straight through
- `status` / `log` — check fetch-scheduler health and inspect a past job's transcript

Every mutating command supports `--dry-run` and a `--select` filter, so you can see the plan before anything touches a repository.

## How do you run it

CLI:

```sh
cargo test --all
cargo run -p git-scylla-cli -- scan ~/UtilCode
cargo run -p git-scylla-cli -- pull ~/UtilCode --dry-run
```

Desktop app, from `apps/desktop`:

```sh
npm install
npm run tauri dev
```

| | |
|---|---|
| `crates/` | Domain, discovery, probing, subprocess discipline, engine |
| `apps/cli/` | `git-scylla` — CLI |
| `apps/desktop/` | The Tauri shell: grid, plan sheet, job drawer |

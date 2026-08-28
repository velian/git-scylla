# git-scylla

A native macOS application for operating on many git repositories at once.

Entirely slop coded, use at your own risk.

```sh
cargo test --all
cargo run -p git-scylla-cli -- scan ~/UtilCode
cargo run -p git-scylla-cli -- pull ~/UtilCode --dry-run
```

The desktop app, from `apps/desktop`:

```sh
npm install
npm run tauri dev
```

| | |
|---|---|
| `crates/` | Domain, discovery, probing, subprocess discipline, engine |
| `apps/cli/` | `git-scylla` — CLI |
| `apps/desktop/` | The Tauri shell: grid, plan sheet, job drawer |

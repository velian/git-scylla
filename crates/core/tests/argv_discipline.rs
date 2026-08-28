//! Every place that spawns `git` must be registered here.
//!
//! The invariant is about **spawn sites**, not about subcommand strings:
//! `filter.rs` legitimately contains `"rebase"` and `"merge"` as keywords, and a
//! test that tripped on those would be deleted within a week.
//!
//! `GitCommand` is the only way to start a subprocess, so an allowlist of every
//! `GitCommand::new` in non-test code accounts for every git invocation in the
//! project. Adding one is then a deliberate act with a diff that says so.
//!
//! It matters because a plan sheet shows the argv, a transcript records it, and
//! a process runs it. Those must be the same three strings.

use std::path::{Path, PathBuf};

/// Files permitted to construct a `GitCommand`, with the reason.
const ALLOWED_SPAWN_SITES: &[(&str, &str)] = &[
    ("crates/exec/src/lib.rs", "defines GitCommand itself"),
    (
        "crates/probe/src/git_cli.rs",
        "the read-only status probe; not an Action, and its argv is one const",
    ),
    (
        "crates/engine/src/runner.rs",
        "runs an Action's own steps, plus `rev-parse --verify HEAD` to record \
         head_before — engine machinery rather than something a user asked for, \
         so it is not and should not be an Action",
    ),
];

fn workspace_root() -> PathBuf {
    // crates/core -> crates -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("workspace root").to_path_buf()
}

/// Every `.rs` file under `crates/*/src` and `apps/*/src`.
///
/// `tests/` directories are excluded: a test may drive `git` however it needs
/// to, and the invariant is about the shipped code.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for group in ["crates", "apps"] {
        let Ok(members) = std::fs::read_dir(root.join(group)) else { continue };
        for m in members.flatten() {
            walk(&m.path().join("src"), &mut out);
            // The Tauri crate's Rust lives under `src-tauri/src`, not `src` —
            // `src` there is TypeScript. Without this the whole desktop crate
            // was exempt from the rule by accident.
            walk(&m.path().join("src-tauri/src"), &mut out);
        }
    }
    out.sort();
    out
}

/// Strip `#[cfg(test)]` modules and doc comments.
///
/// A crude but sufficient cut: in this codebase the test module is the last item
/// in a file, and doc comments are the only other place a `GitCommand::new`
/// appears without being a spawn (the example on `GitCommand` itself).
fn production_code(text: &str) -> String {
    let body = match text.find("#[cfg(test)]") {
        Some(i) => &text[..i],
        None => text,
    };
    body.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//!") && !t.starts_with("///") && !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_git_spawn_site_is_on_the_allowlist() {
    let root = workspace_root();
    let mut unexpected = Vec::new();

    for path in production_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if !production_code(&text).contains("GitCommand::new") {
            continue;
        }
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if !ALLOWED_SPAWN_SITES.iter().any(|(allowed, _)| *allowed == rel) {
            unexpected.push(rel);
        }
    }

    assert!(
        unexpected.is_empty(),
        "these files spawn git without being on the allowlist in {}:\n  {}\n\n\
         If the new site runs an Action, it must take its argv from \
         Action::steps() rather than building one. If it is genuinely a new \
         kind of git invocation, add it to ALLOWED_SPAWN_SITES with the reason.",
        file!(),
        unexpected.join("\n  ")
    );
}

#[test]
fn the_allowlist_does_not_rot() {
    // An entry that no longer spawns git is a stale exemption, and a stale
    // exemption is how the next one gets waved through.
    let root = workspace_root();
    for (rel, why) in ALLOWED_SPAWN_SITES {
        let path = root.join(rel);
        assert!(path.exists(), "allowlisted file {rel} no longer exists ({why})");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("GitCommand::new"),
            "{rel} is allowlisted to spawn git ({why}) but no longer does; remove the entry"
        );
    }
}

/// Trees exempt from the raw-process rule, with the reason.
const RAW_PROCESS_EXEMPT: &[(&str, &str)] = &[
    ("crates/exec/src/", "defines the discipline; it is the thing that applies it"),
    (
        "crates/testkit/src/",
        "builds fixtures, and deliberately uses a *different* environment: pinned \
         author and dates, an isolated HOME, and no production hardening. It is a \
         test-support crate and is never shipped.",
    ),
];

/// Does `needle` appear as its own identifier rather than as a suffix?
///
/// Without this, `Command::new` matches inside `GitCommand::new` and the rule
/// flags exactly the code that is obeying it.
fn contains_identifier(code: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(i) = code[from..].find(needle) {
        let at = from + i;
        let preceded_by_ident =
            code[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !preceded_by_ident {
            return true;
        }
        from = at + needle.len();
    }
    false
}

#[test]
fn no_production_code_reaches_for_a_raw_process() {
    // The hardening, the process group and the deadline apply only if nothing
    // bypasses GitCommand.
    let root = workspace_root();
    let mut offenders = Vec::new();

    for path in production_sources(&root) {
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if RAW_PROCESS_EXEMPT.iter().any(|(prefix, _)| rel.starts_with(prefix)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let code = production_code(&text);
        for needle in ["process::Command", "Command::new", "pre_exec", "libc::kill"] {
            if contains_identifier(&code, needle) {
                offenders.push(format!("{rel}: {needle}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these bypass crates/exec, so the environment hardening and the \
         process-group kill do not apply to them:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_identifier_match_does_not_flag_gitcommand() {
    // The bug this test exists because of: a plain substring search for
    // `Command::new` matches inside `GitCommand::new`, so the rule accuses the
    // code that is obeying it.
    assert!(!contains_identifier("let c = GitCommand::new(p);", "Command::new"));
    assert!(contains_identifier("let c = Command::new(\"git\");", "Command::new"));
    assert!(contains_identifier("std::process::Command::new(x)", "process::Command"));
    assert!(!contains_identifier("my_process::Command::new(x)", "process::Command"));
}

/// `--force` appears nowhere in the shipped source.
///
/// A grep rather than an argv assertion, so that a bare `--force` reaching a
/// spawn by any route — a future action, a helper, a string built somewhere
/// unexpected — fails here rather than being noticed by whoever it happens to.
///
/// A bulk tool that can force-push across forty repositories is one that will
/// eventually do so by accident.
#[test]
fn force_appears_nowhere_in_the_shipped_source() {
    let root = workspace_root();
    let mut found = Vec::new();
    for path in production_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        // Truncated at the inline test module: the invariant is about shipped
        // code, and the assertion that checks for a bare force necessarily
        // contains one. Found by watching this test flag itself.
        let shipped = text.split("#[cfg(test)]").next().unwrap_or(&text);
        for (n, line) in shipped.lines().enumerate() {
            // `--force-with-lease` is the safe half and the whole point of the
            // distinction, so it is matched away before the check.
            let without_lease = line.replace("--force-with-lease", "");
            if without_lease.contains("\"--force\"") || without_lease.contains("\"-f\"") {
                let rel = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
                found.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "a bare force reached the source:\n{}", found.join("\n"));
}

//! The command surface, driven through Tauri's mock runtime.
//!
//! Without this the bridge is only testable by clicking, which means it is only
//! tested when someone remembers to. The mock runtime gives a real invoke path —
//! serialization included — with no window.
//!
//! What these assert is deliberately narrow: that each command reaches the
//! engine and its result crosses the boundary intact. Anything about *what* the
//! engine decides is tested in `crates/engine`, and duplicating it here would be
//! a second specification.

use git_scylla_core::{Action, BatchId, BatchSummary, PullMode};
use git_scylla_desktop_lib::{command_handler, events::UiEvent, state::App};
use git_scylla_engine::{Config, Engine, Selection};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::test::mock_builder;
use tauri::{Manager, WebviewWindow};

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "F")
        .env("GIT_AUTHOR_EMAIL", "f@example.invalid")
        .env("GIT_COMMITTER_NAME", "F")
        .env("GIT_COMMITTER_EMAIL", "f@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repos(dir: &Path, n: usize) -> PathBuf {
    let root = dir.join("repos");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..n {
        let repo = root.join(format!("r{i}"));
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main", "."]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "c1"]);
    }
    root.canonicalize().unwrap()
}

/// A mock app with the real state and the real command handlers.
/// Sole ownership of `GIT_SCYLLA_STATE_DIR` for the duration of one test.
///
/// The variable is process-wide and cargo runs a binary's tests on threads, so
/// four tests each pointing it at their own temporary directory were four tests
/// racing. The symptom was not a flake in whichever set it last but a
/// *different* test failing to save its configuration into a directory that had
/// just been deleted — which is why this is a guard rather than four careful
/// `remove_var` calls at the end of four functions.
///
/// Drop order is load-bearing: the variable is cleared first, then the
/// directory is removed, then the lock is released. Any other order leaves a
/// window in which the next test sees a path that is no longer there.
struct StateDir {
    _dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl StateDir {
    fn new() -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // Poisoning only means an earlier test panicked while holding it, which
        // says nothing about the variable; taking it anyway keeps one failure
        // from cascading into three.
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_SCYLLA_STATE_DIR", dir.path());
        Self { _dir: dir, _lock: lock }
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        std::env::remove_var("GIT_SCYLLA_STATE_DIR");
    }
}

fn app() -> WebviewWindow<tauri::test::MockRuntime> {
    app_with(Default::default())
}

fn app_with(
    config: git_scylla_desktop_lib::config::Config,
) -> WebviewWindow<tauri::test::MockRuntime> {
    // The real context, not `mock_context`: the mock has no resolved ACL, and
    // Tauri then refuses every command with "Plugin not found". Verified that a
    // real build *does* allow them — app commands bypass the ACL, which governs
    // plugin commands — so using the real context is what makes this test agree
    // with the shipped application rather than with a stricter fiction.
    let app = mock_builder()
        .invoke_handler(command_handler!())
        .build(tauri::generate_context!())
        .expect("mock app");
    let engine = tauri::async_runtime::block_on(async { Engine::start(Config::default()) });
    app.manage(App::new(engine, config));
    tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap()
}

/// Poll `f` until it returns `Some`, or give up.
///
/// Bounded on purpose: an unbounded wait turns a broken bridge into a hung test
/// suite, which is strictly worse than a failing one.
fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Some(v) = f() {
            return v;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Invoke a command and get the raw JSON back, so serialization is part of what
/// is being tested rather than something bypassed by calling the function.
fn call(
    window: &WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    body: Value,
) -> Result<Value, Value> {
    let request = tauri::webview::InvokeRequest {
        cmd: cmd.into(),
        callback: tauri::ipc::CallbackFn(0),
        error: tauri::ipc::CallbackFn(1),
        // The origin the capability is scoped to. Windows and Android use
        // `http://tauri.localhost`; everywhere else it is a custom scheme, and
        // getting this wrong is rejected by the ACL as "not allowed".
        url: if cfg!(any(windows, target_os = "android")) {
            "http://tauri.localhost"
        } else {
            "tauri://localhost"
        }
        .parse()
        .unwrap(),
        body: tauri::ipc::InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: tauri::test::INVOKE_KEY.to_string(),
    };
    match tauri::test::get_ipc_response(window, request) {
        Ok(body) => Ok(body.deserialize::<Value>().unwrap()),
        // The error arm is already the serialized `BridgeError` — which is the
        // point of the type.
        Err(value) => Err(value),
    }
}

#[test]
fn every_command_reaches_the_engine_and_its_result_crosses_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    let window = app();

    // Empty before anything is scanned.
    let snaps = call(&window, "get_snapshot", json!({})).expect("get_snapshot");
    assert_eq!(snaps.as_array().unwrap().len(), 0);

    // A scan returns its id, and the id is a plain number on the wire.
    let scan = call(&window, "start_scan", json!({ "roots": [root], "nested": false }))
        .expect("start_scan");
    assert!(scan.is_u64(), "ScanId should be transparent on the wire: {scan}");

    // Wait for the snapshots to land.
    let snaps = eventually("3 repositories to be probed", || {
        let snaps = call(&window, "get_snapshot", json!({})).expect("get_snapshot");
        (snaps.as_array().unwrap().len() == 3).then_some(snaps)
    });

    // The snapshot shape is the generated one, not something reshaped in transit.
    let first = &snaps[0];
    for key in ["id", "path", "kind", "head", "upstream", "remotes", "work", "fetch", "outcome"] {
        assert!(first.get(key).is_some(), "missing {key} in {first}");
    }
    assert_eq!(first["head"]["type"], "Branch");
    assert!(first["probed_at"].is_i64(), "timestamps are millis, not a {{secs,nanos}} map");

    // Plan: an Action and a Selection go out, a plan and its view come back.
    let sheet = call(
        &window,
        "plan",
        json!({
            "action": Action::Pull { mode: PullMode::FfOnly },
            "selection": Selection::All,
        }),
    )
    .expect("plan");
    let plan = &sheet["plan"];
    assert_eq!(plan["skipped"].as_array().unwrap().len(), 3, "no upstreams, so all skipped");
    assert_eq!(plan["eligible"].as_array().unwrap().len(), 0);
    assert_eq!(plan["considered"], 3);

    // ...and the view is the engine's, not something reassembled in transit.
    // Nothing eligible means no confirm control at all.
    let view = &sheet["view"];
    assert_eq!(view["headline"], "Pull 3 repos (ff-only)");
    assert_eq!(view["confirm_label"], serde_json::Value::Null);
    assert_eq!(view["empty_note"], "Nothing to do: no repository in the selection is eligible.");
    assert_eq!(view["skips"][0]["detail"], "no upstream configured");
    assert_eq!(view["skips"][0]["repos"].as_array().unwrap().len(), 3, "expandable to the list");

    // A filter selection travels as its own text.
    let sheet = call(
        &window,
        "plan",
        json!({
            "action": Action::Fetch { prune: false, tags: false },
            "selection": { "type": "Filter", "value": "branch:main" },
        }),
    )
    .expect("plan with a filter");
    assert_eq!(sheet["plan"]["considered"], 3);

    // Refresh and cancel are fire-and-forget but must still round-trip.
    call(&window, "refresh_repo", json!({ "id": first["id"] })).expect("refresh_repo");
    call(&window, "cancel_scan", json!({ "id": scan })).expect("cancel_scan");
}

#[test]
fn a_batch_runs_through_the_bridge_and_its_transcript_comes_back() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    let window = app();
    call(&window, "start_scan", json!({ "roots": [root], "nested": false })).unwrap();
    let snaps = eventually("2 repositories to be probed", || {
        let snaps = call(&window, "get_snapshot", json!({})).unwrap();
        (snaps.as_array().unwrap().len() == 2).then_some(snaps)
    });

    // A commit is something these repositories can actually do.
    let action =
        Action::Commit { message: "from the bridge".into(), stage_all: true, no_verify: false };
    std::fs::write(tmp.path().join("repos/r0/new.txt"), "x\n").unwrap();
    // The engine plans from the snapshot it holds, so a change made behind its
    // back is invisible until something re-reads the repository. That is the
    // point of `refresh_repo`, and planning without it would be planning against
    // state the tool never observed.
    let r0 = snaps
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["path"].as_str().unwrap().ends_with("r0"))
        .unwrap();
    call(&window, "refresh_repo", json!({ "id": r0["id"] })).expect("refresh_repo");

    let sheet = eventually("one repository to become committable", || {
        let sheet = call(&window, "plan", json!({ "action": action, "selection": Selection::All }))
            .unwrap();
        (sheet["plan"]["eligible"].as_array().unwrap().len() == 1).then_some(sheet)
    });
    // The label names the effect, not the selection: one of the two is
    // committable, so the button says one.
    assert_eq!(sheet["view"]["confirm_label"], "Commit in 1 repo (stage all, including untracked)");

    // The plan half goes back untouched. That is the whole reason it crosses at
    // all — the frontend must not be able to edit what it confirmed.
    let batch =
        call(&window, "start_batch", json!({ "plan": sheet["plan"] })).expect("start_batch");
    assert!(batch.is_u64(), "BatchId is transparent on the wire: {batch}");

    // The transcript is reachable by job id.
    //
    // Both ids are tried rather than assuming which is which: the engine
    // allocates skipped jobs first, so the one that actually ran is not id 1.
    // A frontend learns ids from `JobStateChanged`; a test without a webview
    // has to look.
    let log = eventually("the job transcript", || {
        for id in 1..=2 {
            let log = call(&window, "job_log", json!({ "id": id })).expect("job_log");
            if !log.as_array().unwrap().is_empty() {
                return Some(log);
            }
        }
        None
    });
    let line = &log.as_array().unwrap()[0];
    assert!(line.get("at").is_some_and(|v| v.is_i64()));
    assert!(line.get("stream").is_some());
    assert!(line.get("text").is_some());

    call(&window, "cancel_batch", json!({ "id": batch })).expect("cancel_batch");
}

#[test]
fn a_failure_crosses_as_kind_and_message_not_a_debug_string() {
    // Easiest to satisfy by accident and hardest to notice breaking: a
    // `Debug`-formatted Rust error cannot be branched on without parsing prose,
    // and it shows the user type names.
    let tmp = tempfile::tempdir().unwrap();
    repos(tmp.path(), 1);
    let window = app();

    // A malformed filter cannot even be deserialized into a Selection, so this
    // fails at the boundary rather than inside the engine.
    let err = call(
        &window,
        "plan",
        json!({
            "action": Action::Fetch { prune: false, tags: false },
            "selection": { "type": "Filter", "value": "drity" },
        }),
    )
    .expect_err("a bad filter must not become an empty selection");
    let text = err.to_string();
    assert!(text.contains("not a badge"), "the reason should survive the trip: {text}");

    // And the type every command fails with is the documented shape — two named
    // fields, one to branch on and one to read.
    let structured = serde_json::to_value(git_scylla_desktop_lib::BridgeError::new(
        git_scylla_desktop_lib::ErrorKind::EngineStopped,
        "the engine has stopped",
    ))
    .unwrap();
    assert_eq!(structured, json!({ "kind": "EngineStopped", "message": "the engine has stopped" }));
}

#[test]
fn the_command_list_shipped_is_the_command_list_tested() {
    // `command_handler!` is used by both `run()` and the mock app above, so a
    // command cannot be shipped untested or tested but unshipped. This asserts
    // the macro covers what the frontend client calls.
    let source = include_str!("../src/lib.rs");
    for cmd in [
        "start_scan",
        "cancel_scan",
        "get_snapshot",
        "refresh_repo",
        "plan",
        "start_batch",
        "cancel_batch",
        "job_log",
        "pick_root_dir",
    ] {
        assert!(source.contains(&format!("commands::{cmd},")), "{cmd} is not in command_handler!");
    }
}

#[test]
fn roots_persist_across_launches() {
    // A relaunch comes back to the working set rather than to an empty
    // window.
    let _state = StateDir::new();

    let work = tempfile::tempdir().unwrap();
    let root = repos(work.path(), 2);

    {
        let window = app();
        let config = call(&window, "add_root", json!({ "path": root })).expect("add_root");
        assert_eq!(config["roots"].as_array().unwrap().len(), 1);
    }

    // A second "launch" reads what the first wrote.
    let config = git_scylla_desktop_lib::config::load();
    assert_eq!(config.roots, vec![root.clone()]);

    // ...and the command surface agrees.
    let window = app_with(config.clone());
    let seen = call(&window, "get_config", json!({})).expect("get_config");
    assert_eq!(seen["roots"][0].as_str().unwrap(), root.to_str().unwrap());

    let after = call(&window, "remove_root", json!({ "path": root })).expect("remove_root");
    assert_eq!(after["roots"].as_array().unwrap().len(), 0);
    assert!(git_scylla_desktop_lib::config::load().roots.is_empty(), "the removal persisted");
}

#[test]
fn adding_a_root_inside_an_existing_one_returns_the_unchanged_set() {
    // The frontend must never have to guess what the merge rules did, which is
    // why these commands return the configuration rather than `()`.
    let _state = StateDir::new();
    let work = tempfile::tempdir().unwrap();
    let outer = work.path().canonicalize().unwrap();
    let inner = outer.join("repos");
    std::fs::create_dir_all(&inner).unwrap();

    let window = app();
    let config = call(&window, "add_root", json!({ "path": outer })).unwrap();
    assert_eq!(config["roots"].as_array().unwrap().len(), 1);

    let config = call(&window, "add_root", json!({ "path": inner })).unwrap();
    assert_eq!(config["roots"].as_array().unwrap().len(), 1, "nested root rejected");
    assert_eq!(config["roots"][0].as_str().unwrap(), outer.to_str().unwrap());
}

#[test]
fn rows_carry_the_derived_badge_so_the_frontend_never_computes_one() {
    // The grid needs the badge for display and for its default sort; deriving
    // it in TypeScript would put a piece of the domain in the frontend.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    // One dirty, so the two rows do not share a badge.
    std::fs::write(root.join("r0/scratch.txt"), "x\n").unwrap();
    let window = app();
    call(&window, "start_scan", json!({ "roots": [root], "nested": false })).unwrap();

    let rows = eventually("both rows", || {
        let rows = call(&window, "get_snapshot", json!({})).unwrap();
        (rows.as_array().unwrap().len() == 2).then_some(rows)
    });

    for row in rows.as_array().unwrap() {
        assert!(row["badge"].is_string(), "no badge on the row: {row}");
        assert!(row["badge_rank"].is_u64(), "no sort rank on the row: {row}");
        // Flattened: a row is a repository with two extra columns, not a
        // repository wrapped in something.
        assert!(row["path"].is_string(), "the snapshot should be flattened in: {row}");
        assert!(row.get("snapshot").is_none(), "flatten failed: {row}");
    }

    let by_name = |n: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"].as_str().unwrap().ends_with(n))
            .unwrap()
            .clone()
    };
    assert_eq!(by_name("r0")["badge"], "Dirty");
    assert_eq!(by_name("r1")["badge"], "Clean");
    // Worse sorts first, which is what the default ordering relies on.
    assert!(
        by_name("r0")["badge_rank"].as_u64() < by_name("r1")["badge_rank"].as_u64(),
        "dirty should outrank clean"
    );
}

#[test]
fn the_filter_box_is_evaluated_by_the_engines_parser() {
    // One grammar, one implementation. A filter box that reimplemented it in
    // TypeScript would be the second, so the expression makes a round trip.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    std::fs::write(root.join("r1/scratch.txt"), "x\n").unwrap();
    let window = app();
    call(&window, "start_scan", json!({ "roots": [root], "nested": false })).unwrap();
    eventually("three rows", || {
        let rows = call(&window, "get_snapshot", json!({})).unwrap();
        (rows.as_array().unwrap().len() == 3).then_some(())
    });

    let matched = call(&window, "select_repos", json!({ "expr": "dirty" })).expect("select_repos");
    let ids = matched.as_array().unwrap();
    assert_eq!(ids.len(), 1, "exactly the dirty one");
    assert!(ids[0].as_str().unwrap().ends_with("r1"));

    let all = call(&window, "select_repos", json!({ "expr": "branch:main" })).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 3);

    // A malformed expression is an error, never an empty match. Silently
    // matching nothing reads as "your repositories are fine".
    let err = call(&window, "select_repos", json!({ "expr": "drity" })).expect_err("bad expr");
    assert_eq!(err["kind"], "BadSelection", "{err}");
    assert!(err["message"].as_str().unwrap().contains("not a badge"), "{err}");
}

#[test]
fn fetch_now_starts_a_batch_for_exactly_one_repository() {
    // Skipping the plan sheet is allowed for Fetch and nothing else.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    let window = app();
    call(&window, "start_scan", json!({ "roots": [root], "nested": false })).unwrap();
    let rows = eventually("three rows", || {
        let rows = call(&window, "get_snapshot", json!({})).unwrap();
        (rows.as_array().unwrap().len() == 3).then_some(rows)
    });

    // These have no remote, so the job is a skip — but the batch is still for
    // one repository, which is what this asserts.
    let id = rows.as_array().unwrap()[0]["id"].clone();
    let batch = call(&window, "fetch_now", json!({ "id": id })).expect("fetch_now");
    assert!(batch.is_u64());
}

#[test]
fn asking_for_an_editor_that_is_not_configured_says_so() {
    // Distinct from a failure, so the UI can offer to configure one rather than
    // just apologising — and so it does not silently open Finder, which is what
    // the system default for a directory would do and would look like a bug.
    let _state = StateDir::new();
    let prior = std::env::var("EDITOR").ok();
    std::env::remove_var("EDITOR");

    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let window = app();
    let err = call(&window, "hand_off", json!({ "what": "Editor", "path": root.join("r0") }))
        .expect_err("no editor configured");
    assert_eq!(err["kind"], "NotConfigured", "{err}");

    // Configuring one persists and changes the answer.
    let config = call(&window, "set_editor", json!({ "editor": "TextEdit" })).expect("set_editor");
    assert_eq!(config["editor"], "TextEdit");
    assert_eq!(git_scylla_desktop_lib::config::load().editor.as_deref(), Some("TextEdit"));

    if let Some(v) = prior {
        std::env::set_var("EDITOR", v);
    }
}

#[test]
fn the_terminal_is_configurable_and_says_what_it_would_use() {
    // Unlike the editor, an unset terminal is a *working* setting: the
    // resolution always has an answer, because macOS always has Terminal.app.
    // So there is no `NotConfigured` to assert here — what there is instead is
    // an automatic choice, and the thing worth asserting is that it can be seen
    // before it is made. A handoff has no plan sheet to show it in.
    let _state = StateDir::new();

    let window = app();
    let before = call(&window, "resolved_terminal", json!({})).expect("resolved_terminal");
    assert!(before.as_str().is_some_and(|s| !s.is_empty()), "{before}");

    // An explicit choice wins, persists, and is what the dialog then shows.
    let config = call(&window, "set_terminal", json!({ "terminal": "iTerm" })).expect("set");
    assert_eq!(config["terminal"], "iTerm");
    assert_eq!(git_scylla_desktop_lib::config::load().terminal.as_deref(), Some("iTerm"));
    assert_eq!(call(&window, "resolved_terminal", json!({})).unwrap(), "iTerm");

    // Clearing it goes back to the resolution rather than to nothing — the
    // difference from the editor, where clearing leaves the handoff unusable.
    let config = call(&window, "set_terminal", json!({ "terminal": null })).expect("clear");
    assert_eq!(config["terminal"], serde_json::Value::Null);
    assert_eq!(call(&window, "resolved_terminal", json!({})).unwrap(), before);

    // Blank is not a choice either, for the same reason it is not for `editor`.
    let config = call(&window, "set_terminal", json!({ "terminal": "   " })).expect("blank");
    assert_eq!(config["terminal"], serde_json::Value::Null);
}

/// The CLI and the GUI must produce identical plans for the same action and
/// selection.
///
/// Asserted end to end, against the shipped `git-scylla` binary rather than
/// against `engine::plan` — calling the library twice would prove only that a
/// function is deterministic. What can actually drift is a *surface*: a default
/// applied on one side and not the other, a selection assembled differently, a
/// phrase composed locally. This runs both paths over one fixture and compares
/// what each would put in front of a person.
#[test]
fn the_cli_and_the_gui_plan_the_same_thing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = diverse_repos(tmp.path());

    // The GUI path: the `plan` command, through the invoke boundary.
    let window = app();
    call(&window, "start_scan", json!({ "roots": [&root], "nested": false })).unwrap();
    eventually("the fixture to be probed", || {
        let snaps = call(&window, "get_snapshot", json!({})).unwrap();
        (snaps.as_array().unwrap().len() == 4).then_some(())
    });
    let action = Action::Pull { mode: PullMode::FfOnly };
    let sheet = call(&window, "plan", json!({ "action": action, "selection": Selection::All }))
        .expect("plan");
    let view: git_scylla_engine::PlanView =
        serde_json::from_value(sheet["view"].clone()).expect("a PlanView crossed the boundary");

    // The CLI path: the same action and selection, as a person would type them.
    let out = Command::new(cli_binary())
        .args(["pull", "--dry-run", "--mode", "ff-only", &root.to_string_lossy()])
        .output()
        .expect("run git-scylla");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let from_cli = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        view.render(),
        from_cli,
        "the two surfaces disagree about what pulling this fixture would do"
    );
    // Not vacuous: the fixture is chosen so the plan has something to say.
    assert!(view.eligible.is_some(), "{from_cli}");
    assert!(view.skips.len() >= 2, "{from_cli}");
}

/// One repository of each interesting shape, so the compared plan is not just a
/// header. Two behind a shared upstream, one dirty, one with no remote at all.
fn diverse_repos(dir: &Path) -> PathBuf {
    let upstream = dir.join("upstream.git");
    git(dir, &["init", "--bare", "-b", "main", upstream.to_str().unwrap()]);

    let seed = dir.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main", "."]);
    std::fs::write(seed.join("a.txt"), "a\n").unwrap();
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "-m", "c1"]);
    git(&seed, &["remote", "add", "origin", upstream.to_str().unwrap()]);
    git(&seed, &["push", "-u", "origin", "main"]);

    let root = dir.join("repos");
    std::fs::create_dir_all(&root).unwrap();
    for name in ["behind0", "behind1", "dirty"] {
        git(&root, &["clone", upstream.to_str().unwrap(), name]);
    }

    // Move the upstream on, so the clones are behind.
    std::fs::write(seed.join("b.txt"), "b\n").unwrap();
    git(&seed, &["add", "b.txt"]);
    git(&seed, &["commit", "-m", "c2"]);
    git(&seed, &["push", "origin", "main"]);
    for name in ["behind0", "behind1", "dirty"] {
        git(&root.join(name), &["fetch", "origin"]);
    }
    std::fs::write(root.join("dirty/a.txt"), "changed\n").unwrap();

    let lonely = root.join("lonely");
    std::fs::create_dir_all(&lonely).unwrap();
    git(&lonely, &["init", "-b", "main", "."]);
    std::fs::write(lonely.join("a.txt"), "a\n").unwrap();
    git(&lonely, &["add", "a.txt"]);
    git(&lonely, &["commit", "-m", "c1"]);

    root.canonicalize().unwrap()
}

/// The `git-scylla` binary this workspace just built.
///
/// `CARGO_BIN_EXE_*` only covers the current package, and depending on
/// `apps/cli` from `apps/desktop` to get it would invert the layering for the
/// sake of a test. The test binary's own directory is `target/<profile>/deps`,
/// so the sibling verbs live one level up.
fn cli_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps/
    dir.pop(); // <profile>/
    let bin = dir.join("git-scylla");
    assert!(
        bin.exists(),
        "{} is missing. This test compares the two surfaces, so it needs the CLI: \
         run `cargo test --all`, or `cargo build -p git-scylla-cli` first.",
        bin.display()
    );
    bin
}

#[test]
fn the_drawers_banner_is_the_sentence_the_cli_prints() {
    // The same shape because it is the same function: `BatchSummary::render`,
    // projected onto the event the way `Rows` projects a snapshot. Re-assembling
    // it from the counts on the TypeScript side would be a second shape, free to
    // drift from the first.
    let summary = BatchSummary {
        ok: 31,
        failed: 3,
        skipped: 13,
        cancelled: 0,
        pending: 0,
        duration: std::time::Duration::from_millis(4230),
    };
    let ui = UiEvent::from(git_scylla_engine::Event::BatchDone { id: BatchId(1), summary });
    let json = serde_json::to_value(&ui).unwrap();

    assert_eq!(json["type"], "BatchDone", "projected, not passed through as Engine");
    assert_eq!(json["value"]["line"], summary.render());
    assert_eq!(json["value"]["line"], "31 ok, 3 failed, 13 skipped in 4.2s");
    // The counts travel too: the header needs them for progress, and a banner
    // is not a substitute for the numbers behind it.
    assert_eq!(json["value"]["summary"]["failed"], 3);
}

#[test]
fn a_job_event_says_which_batch_and_who_asked() {
    // What the drawer cannot work without. The batch, because the events
    // a user batch emits arrive before `start_batch` has returned its id, so
    // there is nothing else to attribute them to. The origin, because the
    // drawer filters on it — and it filters jobs nobody asked for, which it can
    // therefore never have learned about from a call it made.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    let window = app();
    let mut events = window.state::<App>().engine.subscribe();
    call(&window, "start_scan", json!({ "roots": [root], "nested": false })).unwrap();
    eventually("2 repositories to be probed", || {
        let snaps = call(&window, "get_snapshot", json!({})).unwrap();
        (snaps.as_array().unwrap().len() == 2).then_some(())
    });

    // A commit, so the batch has one job that actually runs and one that skips
    // — the queued announcement is half of what the drawer is built on.
    let action =
        Action::Commit { message: "from the drawer".into(), stage_all: true, no_verify: false };
    std::fs::write(tmp.path().join("repos/r0/new.txt"), "x\n").unwrap();
    let r0 = call(&window, "get_snapshot", json!({}))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["path"].as_str().unwrap().ends_with("r0"))
        .unwrap()
        .clone();
    call(&window, "refresh_repo", json!({ "id": r0["id"] })).expect("refresh_repo");
    let sheet = eventually("one repository to become committable", || {
        let sheet = call(&window, "plan", json!({ "action": action, "selection": Selection::All }))
            .unwrap();
        (sheet["plan"]["eligible"].as_array().unwrap().len() == 1).then_some(sheet)
    });
    let batch =
        call(&window, "start_batch", json!({ "plan": sheet["plan"] })).expect("start_batch");

    let mut seen = 0;
    let mut states: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        let Ok(event) = events.try_recv() else {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        };
        let done = matches!(event, git_scylla_engine::Event::BatchDone { .. });
        // Asserted on the *serialized* form, because that is what the drawer
        // sees. A field that exists in Rust and not on the wire is not a field.
        let json = serde_json::to_value(UiEvent::from(event)).unwrap();
        if json["value"]["type"] == "JobStateChanged" {
            let v = &json["value"]["value"];
            assert_eq!(v["batch"], batch, "a batch job must say which batch");
            assert_eq!(v["origin"], "User");
            states.push(v["state"]["type"].as_str().unwrap().to_string());
            seen += 1;
        }
        if done {
            break;
        }
    }
    assert!(seen >= 4, "only {seen} job events crossed: {states:?}");
    // The queued announcement is the half that is easy to lose: without it the
    // drawer cannot show a row until the scheduler lets the job through.
    assert!(states.contains(&"Queued".to_string()), "{states:?}");
    assert!(states.contains(&"Skipped".to_string()), "{states:?}");
    assert!(states.contains(&"Ok".to_string()), "{states:?}");
}

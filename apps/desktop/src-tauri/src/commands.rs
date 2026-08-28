//! The command surface.
//!
//! **No logic here.** Every one of these sends a `Cmd` and awaits the reply.
//! Anything that decides what is eligible, what runs, or in what order lives in
//! `crates/engine`. Following that rule is why these bodies are one line each:
//! there is nothing else to write.
//!
//! The one exception is `pick_root_dir`, which is not an engine operation at
//! all: it is a native dialog, and the engine has no business knowing about one.

use crate::config::{Config, CustomCommand};
use crate::error::{BridgeError, ErrorKind, Result};
use crate::handoff::{self, Handoff};
use crate::row::RepoRow;
use crate::state::App;
use crate::watch;
use git_scylla_core::{Action, BatchId, JobId, JobOrigin, LogLine, RepoId};
use git_scylla_engine::{Plan, PlanView, ScanId, Selection};
use std::path::PathBuf;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
pub async fn start_scan(app: State<'_, App>, roots: Vec<PathBuf>, nested: bool) -> Result<ScanId> {
    // The watcher is (re)started here rather than at launch, because the roots
    // are what it watches and this is where they arrive. Restarted rather than
    // adjusted: `notify` has no cheap "watch this set instead", and a scan is
    // already the moment the working set is being rebuilt.
    watch::restart(&app, &roots);
    Ok(app.engine.start_scan(roots, nested).await?)
}

#[tauri::command]
pub async fn cancel_scan(app: State<'_, App>, id: ScanId) -> Result<()> {
    Ok(app.engine.cancel_scan(id).await?)
}

/// Every repository the engine knows, as grid rows.
#[tauri::command]
pub async fn get_snapshot(app: State<'_, App>) -> Result<Vec<RepoRow>> {
    Ok(app.engine.snapshot().await?.into_iter().map(RepoRow::from).collect())
}

/// The repositories matching a selection expression.
///
/// The expression is parsed and evaluated **in the engine**, by the same parser
/// the CLI's `--select` uses. One grammar, one implementation — a filter box
/// that reimplemented it in TypeScript would be the second.
#[tauri::command]
pub async fn select_repos(app: State<'_, App>, expr: String) -> Result<Vec<RepoId>> {
    let selection = Selection::parse(&expr, None)
        .map_err(|e| BridgeError::new(ErrorKind::BadSelection, e.to_string()))?;
    Ok(app.engine.select(selection).await?)
}

#[tauri::command]
pub async fn refresh_repo(app: State<'_, App>, id: RepoId) -> Result<()> {
    Ok(app.engine.refresh_repo(id).await?)
}

/// What a batch would do, plus the strings that describe it.
///
/// Both, in one round trip, because they are always wanted together: the sheet
/// shows the view and hands the plan straight back to [`start_batch`]. Deriving
/// the view on this side instead would mean the GUI phrasing "31 will pull"
/// itself, leaving it free to drift from the CLI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PlanSheet {
    /// The domain object. Opaque to the frontend; it goes back to
    /// [`start_batch`] unmodified.
    pub plan: Plan,
    pub view: PlanView,
}

#[tauri::command]
pub async fn plan(app: State<'_, App>, action: Action, selection: Selection) -> Result<PlanSheet> {
    let plan = app.engine.plan(action, selection).await?;
    Ok(PlanSheet { view: plan.view(), plan })
}

#[tauri::command]
pub async fn start_batch(app: State<'_, App>, plan: Plan) -> Result<BatchId> {
    // Always `User`: `Background` belongs to the fetch scheduler, and a surface
    // that could claim it would be able to run without a plan sheet.
    Ok(app.engine.start_batch(plan, JobOrigin::User).await?)
}

/// What undoing a finished batch would do.
///
/// Returns the same `PlanSheet` an action does, and goes through the same sheet
/// — undo is not a special case and must not bypass confirmation. An empty plan
/// renders as "nothing to do" with no control, which is the right answer for a
/// batch that cannot be undone.
#[tauri::command]
pub async fn plan_undo(app: State<'_, App>, id: BatchId) -> Result<PlanSheet> {
    let plan = app.engine.plan_undo(id).await?;
    Ok(PlanSheet { view: plan.view(), plan })
}

#[tauri::command]
pub async fn start_undo(app: State<'_, App>, id: BatchId, plan: Plan) -> Result<BatchId> {
    Ok(app.engine.start_undo(id, plan).await?)
}

#[tauri::command]
pub async fn cancel_batch(app: State<'_, App>, id: BatchId) -> Result<()> {
    Ok(app.engine.cancel_batch(id).await?)
}

#[tauri::command]
pub async fn job_log(app: State<'_, App>, id: JobId) -> Result<Vec<LogLine>> {
    Ok(app.engine.job_log(id).await?)
}

/// The native folder picker.
///
/// `Ok(None)` when the user dismisses it — dismissal is a choice, not a failure,
/// and reporting it as one would put an error toast on screen every time
/// somebody changed their mind.
#[tauri::command]
pub async fn pick_root_dir<R: tauri::Runtime>(window: tauri::Window<R>) -> Result<Option<PathBuf>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    window.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let picked = rx.await.map_err(|_| {
        BridgeError::new(ErrorKind::Cancelled, "the folder picker closed unexpectedly")
    })?;
    Ok(picked.and_then(|p| p.into_path().ok()))
}

// ---- roots -------------------------------------------------------------
//
// Not engine operations: the engine takes roots per scan and has no concept of
// a configured working set. Persisting that choice is the shell's job, which is
// why these are the only commands here that touch anything.

/// The persisted configuration.
#[tauri::command]
pub fn get_config(app: State<'_, App>) -> Result<Config> {
    Ok(app.config().clone())
}

/// Edit the configuration and write it back.
///
/// Every setting command is this: take the lock, change one thing, save if it
/// changed, and hand the whole configuration back so the frontend never has to
/// guess what the merge rules did. Written out seven times, it was seven
/// chances to return a stale copy or to skip the save.
///
/// The closure says whether it changed anything. Returning `false` — an
/// `add_root` for a path already covered, a `remove_custom` for a name that is
/// not there — skips the write rather than rewriting an identical file.
fn edit(app: &App, change: impl FnOnce(&mut Config) -> bool) -> Result<Config> {
    let mut config = app.config();
    if change(&mut config) {
        crate::config::save(&config).map_err(|e| {
            BridgeError::new(ErrorKind::Io, format!("could not save settings: {e}"))
        })?;
    }
    Ok(config.clone())
}

/// Add a root and persist it.
///
/// Returns the configuration as it now stands, so the frontend never has to
/// guess what the merge rules did with a nested path.
#[tauri::command]
pub fn add_root(app: State<'_, App>, path: PathBuf) -> Result<Config> {
    edit(&app, |c| c.add_root(path))
}

#[tauri::command]
pub fn remove_root(app: State<'_, App>, path: PathBuf) -> Result<Config> {
    edit(&app, |c| c.remove_root(&path))
}

/// Open the pane of System Settings that grants Full Disk Access.
///
/// The other half of the highest-value error message in the application: a hint
/// the user cannot act on without hunting through System Settings is most of a
/// hint.
#[tauri::command]
pub fn open_full_disk_access_settings<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<()> {
    const PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
    app.opener().open_url(PANE, None::<&str>).map_err(|e| {
        BridgeError::new(ErrorKind::Io, format!("could not open System Settings: {e}"))
    })
}

// ---- handoffs and per-row actions --------------------------------------

/// Open one repository in another application.
#[tauri::command]
pub fn hand_off<R: tauri::Runtime>(
    handle: tauri::AppHandle<R>,
    app: State<'_, App>,
    what: Handoff,
    path: PathBuf,
) -> Result<()> {
    let (editor, terminal) = {
        let config = app.config();
        (config.editor.clone(), config.terminal.clone())
    };
    handoff::hand_off(&handle, what, &path, editor.as_deref(), terminal.as_deref())
}

/// What `Handoff::Terminal` would use right now.
///
/// Exists so the settings dialog can show it. A handoff has no plan sheet, so
/// this is the only place the automatic choice can be seen before it is made —
/// and an automatic choice nobody can see is the kind this project does not
/// make.
#[tauri::command]
pub fn resolved_terminal(app: State<'_, App>) -> String {
    let configured = app.config().terminal.clone();
    handoff::terminal_app(configured.as_deref())
}

/// The template substitution set.
///
/// Served rather than restated on the other side: help text that repeats a
/// table is help text that goes stale.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Placeholder {
    pub token: String,
    pub means: String,
}

#[tauri::command]
pub fn template_placeholders() -> Vec<Placeholder> {
    git_scylla_core::template::PLACEHOLDERS
        .iter()
        .map(|(token, means)| Placeholder { token: (*token).into(), means: (*means).into() })
        .collect()
}

/// Save or replace a custom command.
#[tauri::command]
pub fn put_custom(app: State<'_, App>, command: CustomCommand) -> Result<Config> {
    edit(&app, |c| {
        c.put_custom(command);
        true
    })
}

#[tauri::command]
pub fn remove_custom(app: State<'_, App>, name: String) -> Result<Config> {
    edit(&app, |c| c.remove_custom(&name))
}

/// Record that the user has read what a custom command does not get.
///
/// Separate from `put_custom` because it is a different act: saving a command
/// is bookkeeping, and acknowledging it is the thing that lets it run. Keeping
/// them apart is what makes "editing the argv clears the acknowledgement"
/// enforceable rather than a convention.
#[tauri::command]
pub fn acknowledge_custom(app: State<'_, App>, name: String) -> Result<Config> {
    edit(&app, |c| match c.custom.iter_mut().find(|c| c.name == name) {
        Some(command) => {
            command.acknowledged = true;
            true
        }
        // Acknowledging a definition that is no longer saved changes nothing,
        // and rewriting the file to record that would be a lie about what
        // happened.
        None => false,
    })
}

#[tauri::command]
pub fn set_editor(app: State<'_, App>, editor: Option<String>) -> Result<Config> {
    edit(&app, |c| {
        c.editor = editor.filter(|s| !s.trim().is_empty());
        true
    })
}

/// Clearing it is meaningful here, and not the same as clearing the editor:
/// `None` means "resolve it", which is a working state.
#[tauri::command]
pub fn set_terminal(app: State<'_, App>, terminal: Option<String>) -> Result<Config> {
    edit(&app, |c| {
        c.terminal = terminal.filter(|s| !s.trim().is_empty());
        true
    })
}

/// Fetch one repository, now.
///
/// Skips the plan sheet, and only `Fetch` may: it is the one action that cannot
/// touch a worktree or local history — the same argument that lets the
/// scheduler fetch on a timer. The exemption is closed to `Fetch`; anything else
/// offered from a row would need its own.
#[tauri::command]
pub async fn fetch_now(app: State<'_, App>, id: RepoId) -> Result<BatchId> {
    let action = Action::Fetch { prune: true, tags: false };
    let plan = app.engine.plan(action, Selection::ids([id])).await?;
    Ok(app.engine.start_batch(plan, JobOrigin::User).await?)
}

/// Tell the menu bar whether anything is selected.
///
/// The window is the only thing that knows, and the menu is the only thing that
/// needs to. Called on the empty↔non-empty transition rather than on every
/// click, so it is a handful of round trips per session.
///
/// Generic over the runtime, like the menu module it calls: the bridge tests
/// drive these through Tauri's `MockRuntime`, and a command hard-wired to `Wry`
/// cannot be tested at all.
#[tauri::command]
pub fn set_has_selection<R: tauri::Runtime>(app: tauri::AppHandle<R>, has: bool) {
    crate::menu::set_has_selection(&app, has);
}

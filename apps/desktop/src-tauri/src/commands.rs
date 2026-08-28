//! The command surface. See `docs/README.md`.

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
    watch::restart(&app, &roots);
    Ok(app.engine.start_scan(roots, nested).await?)
}

#[tauri::command]
pub async fn cancel_scan(app: State<'_, App>, id: ScanId) -> Result<()> {
    Ok(app.engine.cancel_scan(id).await?)
}

#[tauri::command]
pub async fn get_snapshot(app: State<'_, App>) -> Result<Vec<RepoRow>> {
    Ok(app.engine.snapshot().await?.into_iter().map(RepoRow::from).collect())
}

/// The repositories matching a selection expression.
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PlanSheet {
    /// Opaque to the frontend; goes back to [`start_batch`] unmodified.
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
    Ok(app.engine.start_batch(plan, JobOrigin::User).await?)
}

/// What undoing a finished batch would do. Returns the same `PlanSheet` shape
/// as `plan`; undo goes through the same confirmation.
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

/// The native folder picker. `Ok(None)` when the user dismisses it — not an
/// error.
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

/// The persisted configuration.
#[tauri::command]
pub fn get_config(app: State<'_, App>) -> Result<Config> {
    Ok(app.config().clone())
}

/// Take the lock, apply `change`, save if it reports a change, return the
/// resulting configuration.
fn edit(app: &App, change: impl FnOnce(&mut Config) -> bool) -> Result<Config> {
    let mut config = app.config();
    if change(&mut config) {
        crate::config::save(&config).map_err(|e| {
            BridgeError::new(ErrorKind::Io, format!("could not save settings: {e}"))
        })?;
    }
    Ok(config.clone())
}

#[tauri::command]
pub fn add_root(app: State<'_, App>, path: PathBuf) -> Result<Config> {
    edit(&app, |c| c.add_root(path))
}

#[tauri::command]
pub fn remove_root(app: State<'_, App>, path: PathBuf) -> Result<Config> {
    edit(&app, |c| c.remove_root(&path))
}

/// Open the pane of System Settings that grants Full Disk Access.
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

/// What `Handoff::Terminal` would use right now. Lets the settings dialog
/// show the resolution before it happens.
#[tauri::command]
pub fn resolved_terminal(app: State<'_, App>) -> String {
    let configured = app.config().terminal.clone();
    handoff::terminal_app(configured.as_deref())
}

/// The template substitution set.
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
#[tauri::command]
pub fn acknowledge_custom(app: State<'_, App>, name: String) -> Result<Config> {
    edit(&app, |c| match c.custom.iter_mut().find(|c| c.name == name) {
        Some(command) => {
            command.acknowledged = true;
            true
        }
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

/// `None` means "resolve it" — a working state, unlike a cleared editor.
#[tauri::command]
pub fn set_terminal(app: State<'_, App>, terminal: Option<String>) -> Result<Config> {
    edit(&app, |c| {
        c.terminal = terminal.filter(|s| !s.trim().is_empty());
        true
    })
}

/// Fetch one repository, now. Skips the plan sheet — the one action allowed
/// to, since it cannot touch a worktree or local history.
#[tauri::command]
pub async fn fetch_now(app: State<'_, App>, id: RepoId) -> Result<BatchId> {
    let action = Action::Fetch { prune: true, tags: false };
    let plan = app.engine.plan(action, Selection::ids([id])).await?;
    Ok(app.engine.start_batch(plan, JobOrigin::User).await?)
}

/// Tell the menu bar whether anything is selected. Called on the
/// empty↔non-empty transition, not on every click.
#[tauri::command]
pub fn set_has_selection<R: tauri::Runtime>(app: tauri::AppHandle<R>, has: bool) {
    crate::menu::set_has_selection(&app, has);
}

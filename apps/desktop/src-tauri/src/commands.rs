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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PlanSheet {
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

#[tauri::command]
pub fn get_config(app: State<'_, App>) -> Result<Config> {
    Ok(app.config().clone())
}

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

#[tauri::command]
pub fn open_full_disk_access_settings<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<()> {
    const PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
    app.opener().open_url(PANE, None::<&str>).map_err(|e| {
        BridgeError::new(ErrorKind::Io, format!("could not open System Settings: {e}"))
    })
}

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

#[tauri::command]
pub fn resolved_terminal(app: State<'_, App>) -> String {
    let configured = app.config().terminal.clone();
    handoff::terminal_app(configured.as_deref())
}

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

#[tauri::command]
pub fn set_terminal(app: State<'_, App>, terminal: Option<String>) -> Result<Config> {
    edit(&app, |c| {
        c.terminal = terminal.filter(|s| !s.trim().is_empty());
        true
    })
}

#[tauri::command]
pub async fn set_fetch_interval(app: State<'_, App>, secs: Option<u64>) -> Result<Config> {
    let config = edit(&app, |c| {
        c.fetch_interval_secs = secs;
        true
    })?;
    let interval = secs
        .map(std::time::Duration::from_secs)
        .unwrap_or(git_scylla_engine::FetchPolicy::default().interval);
    app.engine.set_fetch_interval(interval).await?;
    Ok(config)
}

#[tauri::command]
pub async fn fetch_now(app: State<'_, App>, id: RepoId) -> Result<BatchId> {
    let action = Action::Fetch { prune: true, tags: false };
    let plan = app.engine.plan(action, Selection::ids([id])).await?;
    Ok(app.engine.start_batch(plan, JobOrigin::User).await?)
}

#[tauri::command]
pub fn set_has_selection<R: tauri::Runtime>(app: tauri::AppHandle<R>, has: bool) {
    crate::menu::set_has_selection(&app, has);
}

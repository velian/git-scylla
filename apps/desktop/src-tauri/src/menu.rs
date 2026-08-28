//! The native menu bar. Every item sends a [`MenuCommand`] to the frontend,
//! which runs the same code path the toolbar or the grid would. None of them
//! reaches the engine directly.

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// The event the frontend listens on for menu selections.
pub const CHANNEL: &str = "menu://command";

/// Every menu command and the id it travels as, expanded into the enum, both
/// directions of the id mapping, and the test list `ALL`.
macro_rules! menu_commands {
    ($($(#[$about:meta])* $variant:ident => $id:literal,)+) => {
        #[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
        /// What a menu item asks the window to do.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum MenuCommand {
            $($(#[$about])* $variant,)+
        }

        /// Every command, in the order it is declared. Tests only.
        #[cfg(test)]
        const ALL: &[MenuCommand] = &[$(MenuCommand::$variant,)+];

        impl MenuCommand {
            fn id(self) -> &'static str {
                match self {
                    $(Self::$variant => $id,)+
                }
            }

            /// Predefined items — Quit, Copy, Fullscreen — reach `dispatch`
            /// too and are not one of ours.
            fn from_id(id: &str) -> Option<Self> {
                match id {
                    $($id => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

menu_commands! {
    AddRoot => "add_root",
    FocusFilter => "focus_filter",
    SelectAll => "select_all",
    ClearSelection => "clear_selection",
    /// Re-probe the selection, or rescan the roots when nothing is selected.
    Refresh => "refresh",
    RescanRoots => "rescan_roots",
    ToggleDrawer => "toggle_drawer",
    Fetch => "fetch",
    PullFfOnly => "pull_ff_only",
    PullRebase => "pull_rebase",
    PullMerge => "pull_merge",
    SortByName => "sort_name",
    SortByPath => "sort_path",
    SortByBranch => "sort_branch",
    SortByState => "sort_state",
    SortByStatus => "sort_status",
    SortByFetch => "sort_fetch",
}

impl MenuCommand {
    /// Does this item need a selection to mean anything? Greyed out rather
    /// than left to silently do nothing.
    fn needs_selection(self) -> bool {
        matches!(
            self,
            Self::ClearSelection
                | Self::Fetch
                | Self::PullFfOnly
                | Self::PullRebase
                | Self::PullMerge
        )
    }
}

/// The items whose enabled state follows the selection, kept so
/// [`set_has_selection`] can find them again.
#[derive(Default)]
pub struct Items<R: Runtime>(pub std::sync::Mutex<Vec<MenuItem<R>>>);

fn item<R: Runtime>(
    app: &AppHandle<R>,
    command: MenuCommand,
    text: &str,
    accelerator: Option<&str>,
    enabled: bool,
) -> tauri::Result<MenuItem<R>> {
    MenuItem::with_id(app, command.id(), text, enabled, accelerator)
}

/// Build the menu and install it.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    use MenuCommand::*;

    let mut gated: Vec<MenuItem<R>> = Vec::new();
    let mut make = |command: MenuCommand, text: &str, accel: Option<&str>| -> tauri::Result<_> {
        let it = item(app, command, text, accel, !command.needs_selection())?;
        if command.needs_selection() {
            gated.push(it.clone());
        }
        Ok(it)
    };

    let file = Submenu::with_items(
        app,
        "File",
        true,
        &[
            &make(AddRoot, "Add Root…", Some("CmdOrCtrl+O"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    // No Select All: ⌘A here means "select every repository", handled by the
    // grid.
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
        ],
    )?;

    let sort = Submenu::with_items(
        app,
        "Sort By",
        true,
        &[
            &make(SortByName, "Name", None)?,
            &make(SortByPath, "Path", None)?,
            &make(SortByBranch, "Branch", None)?,
            &make(SortByState, "State", None)?,
            &make(SortByStatus, "Status", None)?,
            &make(SortByFetch, "Fetch", None)?,
        ],
    )?;

    let view = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &make(FocusFilter, "Filter…", Some("CmdOrCtrl+F"))?,
            &sort,
            &PredefinedMenuItem::separator(app)?,
            &make(ToggleDrawer, "Jobs", Some("CmdOrCtrl+J"))?,
        ],
    )?;

    let pull = Submenu::with_items(
        app,
        "Pull",
        true,
        &[
            &make(PullFfOnly, "Fast-forward only", None)?,
            &make(PullRebase, "Rebase", None)?,
            &make(PullMerge, "Merge", None)?,
        ],
    )?;

    let repo = Submenu::with_items(
        app,
        "Repo",
        true,
        &[
            &make(Fetch, "Fetch", None)?,
            &pull,
            &PredefinedMenuItem::separator(app)?,
            &make(Refresh, "Refresh", Some("CmdOrCtrl+R"))?,
            &make(RescanRoots, "Rescan Roots", Some("CmdOrCtrl+Shift+R"))?,
            &PredefinedMenuItem::separator(app)?,
            &make(SelectAll, "Select All Repositories", None)?,
            &make(ClearSelection, "Clear Selection", None)?,
        ],
    )?;

    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::fullscreen(app, None)?,
        ],
    )?;

    // Named by `productName` and first, so macOS treats it as the app menu.
    let about = tauri::menu::AboutMetadata {
        name: Some("git-scylla".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        comments: Some("Operate on many git repositories at once.".into()),
        ..Default::default()
    };
    let app_menu = Submenu::with_items(
        app,
        "git-scylla",
        true,
        &[
            &PredefinedMenuItem::about(app, None, Some(about))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, None)?,
        ],
    )?;

    let menu = Menu::with_items(app, &[&app_menu, &file, &edit, &view, &repo, &window])?;
    app.set_menu(menu)?;
    app.manage(Items(std::sync::Mutex::new(gated)));
    Ok(())
}

/// Forward a menu selection to the window.
pub fn dispatch<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let Some(command) = MenuCommand::from_id(event.id().as_ref()) else { return };
    let _ = app.emit(CHANNEL, command);
}

/// Grey out the items that need a selection, or restore them.
pub fn set_has_selection<R: Runtime>(app: &AppHandle<R>, has: bool) {
    let Some(items) = app.try_state::<Items<R>>() else { return };
    let Ok(items) = items.0.lock() else { return };
    for it in items.iter() {
        let _ = it.set_enabled(has);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_distinct_id_that_survives_the_round_trip() {
        let mut ids: Vec<&str> = ALL.iter().map(|c| c.id()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two menu items share an id");

        for &command in ALL {
            assert_eq!(MenuCommand::from_id(command.id()), Some(command));
        }
    }

    #[test]
    fn an_unknown_id_is_not_a_command() {
        assert_eq!(MenuCommand::from_id("quit"), None);
        assert_eq!(MenuCommand::from_id(""), None);
    }

    #[test]
    fn only_the_items_that_act_on_a_selection_are_gated() {
        let gated: Vec<&str> = ALL.iter().filter(|c| c.needs_selection()).map(|c| c.id()).collect();
        assert_eq!(
            gated,
            ["clear_selection", "fetch", "pull_ff_only", "pull_rebase", "pull_merge"]
        );

        for command in [MenuCommand::AddRoot, MenuCommand::RescanRoots, MenuCommand::SortByName] {
            assert!(!command.needs_selection(), "{command:?}");
        }
    }
}

//! The native menu bar.
//!
//! An app with an empty menu bar reads as a web page, and this one is a Mac
//! application. The menu is also the only place the keyboard shortcuts are
//! *discoverable*: a binding nobody can find is a binding nobody uses.
//!
//! **Every item here is a second route to something the window already does.**
//! None of them reaches the engine. They send a [`MenuCommand`] to the frontend,
//! which runs the same code path the toolbar or the grid would — because two
//! routes to one action would be two places for the action to be wrong.

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// The event the frontend listens on for menu selections.
pub const CHANNEL: &str = "menu://command";

/// Every menu command, and the id it travels as.
///
/// One list, expanded into the enum, both directions of the id mapping, and
/// `ALL`. Those were three hand-kept lists over the same seventeen variants,
/// held together by a runtime assert in `install` and a test whose only job was
/// to notice when they had diverged. Now a command in the list is reachable by
/// construction and one that is not does not exist — and a duplicate id is an
/// unreachable arm in `from_id`, which is a build failure rather than two menu
/// items quietly sharing an action.
macro_rules! menu_commands {
    ($($(#[$about:meta])* $variant:ident => $id:literal,)+) => {
        #[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
        /// What a menu item asks the window to do.
        ///
        /// An enum rather than loose string ids, generated into TypeScript like
        /// everything else that crosses the boundary: adding an item then fails
        /// to compile on the other side until it is handled, which is the only
        /// reliable way to keep a menu from growing a dead entry.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum MenuCommand {
            $($(#[$about])* $variant,)+
        }

        /// Every command, in the order it is declared.
        ///
        /// Tests only. Nothing at runtime sweeps the list any more — `id` and
        /// `from_id` are exhaustive matches the compiler checks — so a
        /// non-test build that carried it would be carrying dead weight.
        #[cfg(test)]
        const ALL: &[MenuCommand] = &[$(MenuCommand::$variant,)+];

        impl MenuCommand {
            /// The menu-item id this travels as.
            fn id(self) -> &'static str {
                match self {
                    $(Self::$variant => $id,)+
                }
            }

            /// Predefined items — Quit, Copy, Fullscreen — reach `dispatch`
            /// too. They are handled by the system and must not be mistaken for
            /// one of ours, which is what the `None` is for.
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
    /// Re-probe the selection, or rescan the roots when nothing is selected —
    /// the same thing the toolbar's Refresh does.
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
    /// Does this item need a selection to mean anything?
    ///
    /// Greyed out rather than silently doing nothing when it does not: an item
    /// that looks available and then ignores you is worse than one that says it
    /// is unavailable.
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
        // Selection-gated items start disabled, because nothing is selected
        // when the window opens.
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

    // No Select All here, deliberately. The predefined one takes ⌘A for the
    // focused text field, and ⌘A in this window means "select every repository"
    // — the grid's handler already leaves text fields alone.
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

    // The application menu. Named by `productName`, and first, which is what
    // makes macOS treat it as one.
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
    // Predefined items (Quit, Copy, Fullscreen…) are handled by the system and
    // have no command of their own. Not an error.
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
        // `dispatch` gets a string back from the system and has nothing else to
        // go on. A duplicate id would route two items to one action; a missing
        // one would make an item inert.
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
        // Predefined items — Quit, Copy, Fullscreen — arrive here too. They are
        // handled by the system and must not be mistaken for one of ours.
        assert_eq!(MenuCommand::from_id("quit"), None);
        assert_eq!(MenuCommand::from_id(""), None);
    }

    #[test]
    fn only_the_items_that_act_on_a_selection_are_gated() {
        // Spelled as ids rather than as a second copy of the `matches!`, so it
        // reads as the list of what greys out rather than as the code under
        // test written down twice.
        let gated: Vec<&str> = ALL.iter().filter(|c| c.needs_selection()).map(|c| c.id()).collect();
        assert_eq!(
            gated,
            ["clear_selection", "fetch", "pull_ff_only", "pull_rebase", "pull_merge"]
        );

        // The rest have to keep working with nothing selected: adding a root
        // and rescanning are how you *get* a selection, and an item greyed out
        // before you can use it is an item you cannot use.
        for command in [MenuCommand::AddRoot, MenuCommand::RescanRoots, MenuCommand::SortByName] {
            assert!(!command.needs_selection(), "{command:?}");
        }
    }
}

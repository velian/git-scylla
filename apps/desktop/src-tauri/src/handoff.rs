//! Handing a repository to another application.
//!
//! Anything the engine runs is non-interactive; anything interactive is not run
//! by the engine. These are not `Action`s and produce no `Job` — no transcript,
//! no state, no undo, and no pretence that the tool knows what happened next.
//! One repository at a time.
//!
//! Everything goes through `tauri-plugin-opener`, which is `open(1)` underneath.
//! Spawning it directly would work equally well and would be the only raw
//! subprocess in the project outside `crates/exec`; using the plugin keeps that
//! rule absolute rather than nearly absolute.

use crate::error::{BridgeError, ErrorKind, Result};
use serde::Deserialize;
use std::path::Path;
use tauri_plugin_opener::OpenerExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Handoff {
    Finder,
    Terminal,
    Editor,
}

/// Open `repo` in another application.
///
/// `editor` is the configured application name; `$EDITOR` is consulted as a
/// fallback but is usually a terminal program (`vim`, `nano`) rather than
/// something `open -a` understands, so a repository with neither reports that
/// rather than silently opening Finder — which is what the system default for a
/// directory would do, duplicating "Reveal in Finder" and looking like a bug.
pub fn hand_off<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    what: Handoff,
    repo: &Path,
    editor: Option<&str>,
    terminal: Option<&str>,
) -> Result<()> {
    let path = repo.to_string_lossy().to_string();
    match what {
        Handoff::Finder => app
            .opener()
            .reveal_item_in_dir(repo)
            .map_err(|e| io(format!("could not reveal {path}: {e}"))),
        // Never `NotConfigured`, unlike the editor: `terminal_app` always has
        // an answer, because macOS always has Terminal.app.
        Handoff::Terminal => open_with(app, &path, &terminal_app(terminal)),
        Handoff::Editor => {
            let app_name = editor
                .map(str::to_string)
                .or_else(|| std::env::var("EDITOR").ok())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    BridgeError::new(
                        ErrorKind::NotConfigured,
                        "no editor configured, and $EDITOR is unset",
                    )
                })?;
            open_with(app, &path, &app_name)
        }
    }
}

/// Terminals worth looking for, best first.
///
/// A curated list rather than a search, because "an application that opens a
/// directory" is also every editor and file manager on the machine. Ordered so
/// that a third-party terminal beats `Terminal`: somebody who installed Ghostty
/// did not do it in order to keep using Terminal.app, and `Terminal` is last
/// only because it is the one that is always there.
///
/// Membership here is not enough on its own — see [`handles_directories`].
/// Adding a terminal that ignores a directory argument would produce the worst
/// possible outcome, a window opening in `$HOME` with no error, so a candidate
/// has to say it handles one before it is chosen.
const KNOWN_TERMINALS: &[&str] = &["Ghostty", "iTerm", "WezTerm", "kitty", "Warp", "Terminal"];

/// Where macOS keeps applications, in the order Launch Services prefers.
fn app_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![
        std::path::PathBuf::from("/Applications"),
        std::path::PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.insert(0, std::path::Path::new(&home).join("Applications"));
    }
    dirs
}

/// Which application `Handoff::Terminal` should use.
///
/// In order:
///
/// 1. What the user configured. An explicit choice is never second-guessed, and
///    is not checked for existence either — a typo produces `open`'s own error,
///    which names the application, and silently substituting something else
///    would be worse than a message.
/// 2. `$TERM_PROGRAM`, when it names something installed. The strongest
///    automatic signal there is, because it *is* the terminal the user is
///    sitting in — but weak in practice: a GUI application launched from the
///    Dock or Finder inherits no such variable, so it is set only when the app
///    was started from a shell, which means development.
/// 3. The first known terminal that is installed and handles directories.
/// 4. `Terminal`, which macOS always has.
///
/// Steps 2 and 3 are guesses, and a handoff has no plan sheet to show one in —
/// so the settings dialog displays what this resolved to. No automatic choice
/// goes unseen.
pub fn terminal_app(configured: Option<&str>) -> String {
    if let Some(name) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    if let Some(from_env) = term_program().filter(|name| installed(name).is_some()) {
        return from_env;
    }
    KNOWN_TERMINALS
        .iter()
        .find(|name| installed(name).is_some_and(|bundle| handles_directories(&bundle)))
        .map_or_else(|| "Terminal".to_string(), |name| (*name).to_string())
}

/// `$TERM_PROGRAM` as an application name.
///
/// The values are not application names and are not consistent, which is why
/// this is a table and not a `to_string`. Anything unrecognised is passed
/// through and then checked against what is installed, so a terminal nobody
/// here has heard of still works if it names itself after its bundle.
fn term_program() -> Option<String> {
    let raw = std::env::var("TERM_PROGRAM").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(match raw {
        "Apple_Terminal" => "Terminal".to_string(),
        // Set by an *editor's* integrated terminal. Handing a repository to it
        // would open the editor, which is the other handoff and not this one.
        "vscode" | "Hyper" | "JetBrains-JediTerm" => return None,
        other => other.trim_end_matches(".app").to_string(),
    })
}

/// The bundle for an `open -a` name, if it is on this machine.
fn installed(name: &str) -> Option<std::path::PathBuf> {
    app_dirs().into_iter().map(|dir| dir.join(format!("{name}.app"))).find(|p| p.is_dir())
}

/// Does this application say it can be given a directory?
///
/// A bundle that declares `public.directory` among its document types is one
/// `open -a <app> <dir>` will actually open *in* that directory. One that does
/// not will open in `$HOME` and report success, which is the failure worth
/// designing out: a silent wrong answer with nothing to read.
///
/// The check is a substring search over `Info.plist` rather than a plist parse.
/// The type name is stored literally in both the XML and the binary format, and
/// pulling in a plist parser to confirm which key it sits under would be a
/// dependency for a stricter answer than this needs — a bundle that mentions
/// `public.directory` at all is one that has thought about directories.
fn handles_directories(bundle: &Path) -> bool {
    std::fs::read(bundle.join("Contents/Info.plist"))
        .map(|bytes| find(&bytes, b"public.directory"))
        .unwrap_or(false)
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn open_with<R: tauri::Runtime>(app: &tauri::AppHandle<R>, path: &str, with: &str) -> Result<()> {
    app.opener()
        .open_path(path, Some(with))
        .map_err(|e| io(format!("could not open {path} with {with}: {e}")))
}

fn io(message: String) -> BridgeError {
    BridgeError::new(ErrorKind::Io, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TERM_PROGRAM` is process-wide, and three tests below read or write it.
    /// Cargo runs tests in one binary on threads, so without this they race —
    /// one clearing the variable while another has just set it. Cheaper than
    /// `--test-threads=1`, which would slow every other test in the crate to
    /// fix a problem three of them have.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A fake `.app` bundle, so the resolution can be tested without depending
    /// on what happens to be installed on the machine running the tests.
    fn bundle(dir: &Path, name: &str, info: &str) {
        let contents = dir.join(format!("{name}.app/Contents"));
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(contents.join("Info.plist"), info).unwrap();
    }

    const HANDLES_DIRS: &str =
        "<plist><key>LSItemContentTypes</key><string>public.directory</string></plist>";
    const DOES_NOT: &str =
        "<plist><key>LSItemContentTypes</key><string>public.plain-text</string></plist>";

    #[test]
    fn a_configured_terminal_is_never_second_guessed() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        // Not even checked for existence. A typo produces `open`'s own error,
        // which names the application; silently substituting something else
        // would leave the user with a working button and a wrong terminal, and
        // nothing to read about why.
        assert_eq!(terminal_app(Some("iTerm")), "iTerm");
        assert_eq!(terminal_app(Some("NoSuchTerminal")), "NoSuchTerminal");
        // Blank is not a choice.
        assert_eq!(terminal_app(Some("   ")), terminal_app(None));
    }

    #[test]
    fn a_bundle_that_does_not_handle_directories_is_not_a_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path(), "Ghostty", DOES_NOT);
        bundle(tmp.path(), "Terminal", HANDLES_DIRS);
        assert!(!handles_directories(&tmp.path().join("Ghostty.app")));
        assert!(handles_directories(&tmp.path().join("Terminal.app")));
        // A bundle with no `Info.plist` at all is not a candidate either.
        std::fs::create_dir_all(tmp.path().join("Empty.app")).unwrap();
        assert!(!handles_directories(&tmp.path().join("Empty.app")));
    }

    #[test]
    fn the_real_terminal_app_declares_directory_support() {
        // The check is a substring search rather than a plist parse, so it is
        // worth pinning against a real bundle: macOS always has this one, and
        // if the shape of `Info.plist` ever changes underneath the search this
        // is what says so.
        let bundle = Path::new("/System/Applications/Utilities/Terminal.app");
        if !bundle.is_dir() {
            return; // not macOS, or a stripped image
        }
        assert!(handles_directories(bundle));
        // ...and something that is emphatically not a terminal does not.
        let calc = Path::new("/System/Applications/Calculator.app");
        if calc.is_dir() {
            assert!(!handles_directories(calc));
        }
    }

    #[test]
    fn an_editors_integrated_terminal_is_not_a_terminal_to_hand_off_to() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("TERM_PROGRAM").ok();
        // `TERM_PROGRAM=vscode` means the app was launched from a terminal
        // *inside* an editor. Handing a repository to it would open the editor,
        // which is the other handoff, and would silently make two buttons do
        // the same thing.
        for value in ["vscode", "Hyper", "JetBrains-JediTerm"] {
            std::env::set_var("TERM_PROGRAM", value);
            assert_eq!(term_program(), None, "{value}");
        }
        std::env::set_var("TERM_PROGRAM", "Apple_Terminal");
        assert_eq!(term_program().as_deref(), Some("Terminal"));
        // Unrecognised values are passed through: a terminal nobody here has
        // heard of still works if it names itself after its bundle. `installed`
        // is what then decides.
        std::env::set_var("TERM_PROGRAM", "iTerm.app");
        assert_eq!(term_program().as_deref(), Some("iTerm"));
        std::env::set_var("TERM_PROGRAM", "  ");
        assert_eq!(term_program(), None);
        std::env::remove_var("TERM_PROGRAM");
        assert_eq!(term_program(), None);
        if let Some(prior) = prior {
            std::env::set_var("TERM_PROGRAM", prior);
        }
    }

    #[test]
    fn there_is_always_an_answer() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        // The reason `Handoff::Terminal` never reports `NotConfigured` the way
        // the editor does: macOS always has Terminal.app, so the chain cannot
        // run out — with or without a `TERM_PROGRAM` to read.
        let prior = std::env::var("TERM_PROGRAM").ok();
        std::env::remove_var("TERM_PROGRAM");
        assert!(!terminal_app(None).is_empty());
        std::env::set_var("TERM_PROGRAM", "Apple_Terminal");
        assert!(!terminal_app(None).is_empty());
        match prior {
            Some(v) => std::env::set_var("TERM_PROGRAM", v),
            None => std::env::remove_var("TERM_PROGRAM"),
        }
    }
}

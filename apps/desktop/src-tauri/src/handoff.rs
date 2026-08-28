//! Handing a repository to another application. Not an `Action` and produces
//! no `Job` — no transcript, no undo. One repository at a time.
//!
//! Goes through `tauri-plugin-opener` (`open(1)` underneath), the crate's
//! only subprocess boundary outside `crates/exec`.

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

/// Open `repo` in another application. `editor` is the configured application
/// name; `$EDITOR` is consulted as a fallback. A repository with neither
/// reports `NotConfigured` rather than opening Finder.
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

/// Terminals worth looking for, third-party first, `Terminal` last. Membership
/// alone is not enough — see [`handles_directories`].
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

/// Which application `Handoff::Terminal` should use, in order: what the user
/// configured (unchecked); `$TERM_PROGRAM`, if installed; the first known
/// terminal installed that handles directories; `Terminal`.
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

/// `$TERM_PROGRAM` as an application name. Unrecognised values pass through
/// and are checked against what is installed.
fn term_program() -> Option<String> {
    let raw = std::env::var("TERM_PROGRAM").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(match raw {
        "Apple_Terminal" => "Terminal".to_string(),
        // An editor's integrated terminal — not a terminal to hand off to.
        "vscode" | "Hyper" | "JetBrains-JediTerm" => return None,
        other => other.trim_end_matches(".app").to_string(),
    })
}

/// The bundle for an `open -a` name, if it is on this machine.
fn installed(name: &str) -> Option<std::path::PathBuf> {
    app_dirs().into_iter().map(|dir| dir.join(format!("{name}.app"))).find(|p| p.is_dir())
}

/// Does this application declare `public.directory` among its document
/// types? A substring search over `Info.plist`, not a plist parse.
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

    /// Serializes the tests below that read or write the process-wide
    /// `TERM_PROGRAM`.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        assert_eq!(terminal_app(Some("iTerm")), "iTerm");
        assert_eq!(terminal_app(Some("NoSuchTerminal")), "NoSuchTerminal");
        assert_eq!(terminal_app(Some("   ")), terminal_app(None));
    }

    #[test]
    fn a_bundle_that_does_not_handle_directories_is_not_a_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path(), "Ghostty", DOES_NOT);
        bundle(tmp.path(), "Terminal", HANDLES_DIRS);
        assert!(!handles_directories(&tmp.path().join("Ghostty.app")));
        assert!(handles_directories(&tmp.path().join("Terminal.app")));
        std::fs::create_dir_all(tmp.path().join("Empty.app")).unwrap();
        assert!(!handles_directories(&tmp.path().join("Empty.app")));
    }

    #[test]
    fn the_real_terminal_app_declares_directory_support() {
        let bundle = Path::new("/System/Applications/Utilities/Terminal.app");
        if !bundle.is_dir() {
            return;
        }
        assert!(handles_directories(bundle));
        let calc = Path::new("/System/Applications/Calculator.app");
        if calc.is_dir() {
            assert!(!handles_directories(calc));
        }
    }

    #[test]
    fn an_editors_integrated_terminal_is_not_a_terminal_to_hand_off_to() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("TERM_PROGRAM").ok();
        for value in ["vscode", "Hyper", "JetBrains-JediTerm"] {
            std::env::set_var("TERM_PROGRAM", value);
            assert_eq!(term_program(), None, "{value}");
        }
        std::env::set_var("TERM_PROGRAM", "Apple_Terminal");
        assert_eq!(term_program().as_deref(), Some("Terminal"));
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

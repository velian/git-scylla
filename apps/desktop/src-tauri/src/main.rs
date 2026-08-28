// Prevents an extra console window on Windows in release. Irrelevant to a
// macOS-only application, but removing it is the kind of edit that surprises
// someone who later tries a cross build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    git_scylla_desktop_lib::run()
}

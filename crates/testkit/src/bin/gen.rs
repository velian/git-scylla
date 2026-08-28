//! Build the fixture set into a directory and print what it contains.
//!
//! `cargo run -p git-scylla-testkit --bin gen -- /tmp/fixtures`
//!
//! For manual poking, and for the first pass of writing an expectation: run it,
//! look at the repositories, then encode what git actually produced rather than
//! what one assumed it would.

use git_scylla_testkit::FixtureSet;

fn main() -> std::process::ExitCode {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: gen <dir>");
        return std::process::ExitCode::from(2);
    };
    let dir = std::path::PathBuf::from(dir);
    match FixtureSet::build(&dir) {
        Ok(set) => {
            println!("built {} fixtures", set.fixtures.len());
            println!("scan root: {}", set.scan_root.display());
            for f in &set.fixtures {
                println!(
                    "  {:<26} {}{}",
                    f.name,
                    f.path.strip_prefix(&set.scan_root).unwrap_or(&f.path).display(),
                    if f.nested_only { "   (nested only)" } else { "" }
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

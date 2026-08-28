use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
#[error("git {args:?} in {cwd} failed ({code}): {stderr}")]
pub struct GitError {
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub code: i32,
    pub stderr: String,
}

/// A `git` invoker with environment and config pinned for byte-reproducible
/// fixtures across machines and runs.
pub struct Git {
    home: PathBuf,
}

const DATE: &str = "2024-01-01T00:00:00+00:00";

impl Git {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        let mut cmd = Command::new("git");
        cmd.args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "advice.detachedHead=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .current_dir(cwd)
        .env("HOME", &self.home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");

        let out = cmd.output().map_err(|e| GitError {
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_path_buf(),
            code: -1,
            stderr: e.to_string(),
        })?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(GitError {
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.to_path_buf(),
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// Succeeds only if `args` fails.
    pub fn run_expect_failure(&self, cwd: &Path, args: &[&str]) -> Result<(), GitError> {
        match self.run(cwd, args) {
            Ok(_) => Err(GitError {
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.to_path_buf(),
                code: 0,
                stderr: "expected this to fail, but it succeeded".into(),
            }),
            Err(_) => Ok(()),
        }
    }
}

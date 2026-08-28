use tokio::process::Command;

/// The environment every child inherits, unconditionally.
///
/// This table is the single most load-bearing thing in the crate, and it is the
/// kind of thing that silently regresses — so a unit test asserts the full
/// constructed environment, not just that the function was called.
///
/// | Variable | Without it |
/// |---|---|
/// | `GIT_TERMINAL_PROMPT=0` | An HTTPS remote needing credentials waits on a terminal read, invisibly, forever. |
/// | `GIT_ASKPASS=/usr/bin/false` | Git falls back to a GUI or helper askpass, which can block or pop a dialog behind the app. |
/// | `SSH_ASKPASS_REQUIRE=never` | `ssh` reaches for an askpass program of its own, outside git's control. |
/// | `GIT_EDITOR=true` | A merge commit opens an editor and waits for it to exit. |
/// | `GIT_SEQUENCE_EDITOR=true` | An interactive rebase opens a todo list and waits. Worse: it could *start* one. |
/// | `LC_ALL=C` | Error strings are localised, and failures are explained by matching on them. |
///
/// Setting `GIT_EDITOR=true` rather than `false` is deliberate: `true` exits 0,
/// so git treats the message as accepted and proceeds. `false` would make every
/// merge commit fail, which is a different bug rather than a fix.
pub const HARDENED_ENV: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", "/usr/bin/false"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    ("LC_ALL", "C"),
];

/// Apply [`HARDENED_ENV`]. Called last, so nothing a caller set can win.
pub fn harden(cmd: &mut Command) {
    for (k, v) in HARDENED_ENV {
        cmd.env(k, v);
    }
}

#[cfg(test)]
mod tests {
    use crate::GitCommand;
    use std::collections::BTreeMap;

    /// The environment overrides a constructed command actually carries.
    ///
    /// `get_envs` reports only explicit changes, not the inherited environment,
    /// which is exactly the set under test: git-scylla does **not** clear the
    /// environment, because a child that cannot see `PATH`, `SSH_AUTH_SOCK` or
    /// the user's credential helpers cannot do its job.
    fn env_of(cmd: &GitCommand) -> BTreeMap<String, Option<String>> {
        let mut tokio_cmd = tokio::process::Command::new("git");
        for (k, v) in &cmd.extra_env {
            tokio_cmd.env(k, v);
        }
        super::harden(&mut tokio_cmd);
        tokio_cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
            })
            .collect()
    }

    #[test]
    fn the_full_environment_of_a_constructed_command() {
        let env = env_of(&GitCommand::new("/tmp"));
        let expected: BTreeMap<String, Option<String>> =
            super::HARDENED_ENV.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect();
        // Equality, not containment: an extra variable is as much a regression
        // as a missing one, because it means something is being configured
        // somewhere other than this table.
        assert_eq!(env, expected);
        assert_eq!(env.len(), 6);
    }

    #[test]
    fn the_hardening_cannot_be_overridden() {
        // Whether by mistake or by clever reasoning about one special
        // repository. The guarantee is only worth having unconditionally.
        let cmd = GitCommand::new("/tmp")
            .env("GIT_TERMINAL_PROMPT", "1")
            .env("GIT_ASKPASS", "/usr/bin/ssh-askpass")
            .env("GIT_SEQUENCE_EDITOR", "vim")
            .env("LC_ALL", "de_DE.UTF-8");
        let env = env_of(&cmd);
        assert_eq!(env["GIT_TERMINAL_PROMPT"].as_deref(), Some("0"));
        assert_eq!(env["GIT_ASKPASS"].as_deref(), Some("/usr/bin/false"));
        assert_eq!(env["GIT_SEQUENCE_EDITOR"].as_deref(), Some("true"));
        assert_eq!(env["LC_ALL"].as_deref(), Some("C"));
    }

    #[test]
    fn extra_variables_are_kept_alongside_the_hardening() {
        // The probe needs GIT_OPTIONAL_LOCKS; tests need GIT_CONFIG_GLOBAL.
        // Neither collides with the table, and both must survive.
        let cmd = GitCommand::new("/tmp")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        let env = env_of(&cmd);
        assert_eq!(env["GIT_OPTIONAL_LOCKS"].as_deref(), Some("0"));
        assert_eq!(env["GIT_CONFIG_GLOBAL"].as_deref(), Some("/dev/null"));
        assert_eq!(env["GIT_TERMINAL_PROMPT"].as_deref(), Some("0"));
        assert_eq!(env.len(), 8);
    }

    #[test]
    fn git_editor_exits_zero_so_a_merge_commit_is_accepted() {
        // `false` would make every merge commit fail, which is a different bug
        // rather than a fix. Asserted because "disable the editor" reads like it
        // should be `false`.
        let table: std::collections::HashMap<_, _> = super::HARDENED_ENV.iter().copied().collect();
        assert_eq!(table["GIT_EDITOR"], "true");
        assert_eq!(table["GIT_SEQUENCE_EDITOR"], "true");
    }
}

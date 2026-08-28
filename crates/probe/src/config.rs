//! Reading remote names and hosts out of `config`.
//!
//! A file read rather than `git remote -v`, since the host is only a
//! concurrency-bucket key for automatic fetching. Two consequences:
//!
//! * `url.<base>.insteadOf` rewrites are not applied; a rewritten URL buckets
//!   under the host it is written as.
//! * `[include]` / `[includeIf]` directives are not followed; a remote
//!   defined only in an included file is invisible here.

use git_scylla_core::Remote;
use std::path::Path;

/// Parse the `[remote "<name>"] url = ...` stanzas of a git config file.
///
/// A minimal INI reader, not a general one: only remote names and URLs.
pub fn parse_remotes(config_path: &Path) -> Vec<Remote> {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    parse_remotes_str(&text)
}

pub(crate) fn parse_remotes_str(text: &str) -> Vec<Remote> {
    let mut out: Vec<Remote> = Vec::new();
    let mut current: Option<String> = None;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = remote_name(section);
            if let Some(name) = &current {
                if !out.iter().any(|r| &r.name == name) {
                    out.push(Remote { name: name.clone(), host: None });
                }
            }
            continue;
        }
        let Some(name) = &current else { continue };
        let Some((key, value)) = line.split_once('=') else { continue };
        // `pushurl` is ignored: fetching is what buckets by host.
        if key.trim().eq_ignore_ascii_case("url") {
            let host = host_of_url(unquote(value.trim()));
            if let Some(r) = out.iter_mut().find(|r| &r.name == name) {
                if r.host.is_none() {
                    r.host = host;
                }
            }
        }
    }
    out
}

/// `remote "origin"` -> `origin`. Anything else is not a remote section.
fn remote_name(section: &str) -> Option<String> {
    let rest = section.trim().strip_prefix("remote")?.trim_start();
    let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.is_empty()).then(|| inner.to_string())
}

fn strip_comment(line: &str) -> &str {
    match line.find(['#', ';']) {
        Some(i) => &line[..i],
        None => line,
    }
}

fn unquote(v: &str) -> &str {
    v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v)
}

/// Extract the host from a git remote URL.
///
/// Handles the three forms git accepts: a URL with a scheme, the `scp`-like
/// `[user@]host:path`, and a local path. Local paths and `file://` yield
/// `None`.
pub fn host_of_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        // Userinfo may itself contain '@' in a password, so split at the last.
        let hostport = match authority.rsplit_once('@') {
            Some((_, h)) => h,
            None => authority,
        };
        return normalise_host(hostport);
    }
    if url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
        return None;
    }
    // scp-like: the colon must precede any slash, or it's a path with a
    // colon in a directory name.
    let colon = url.find(':')?;
    if url[..colon].contains('/') {
        return None;
    }
    let hostpart = match url[..colon].rsplit_once('@') {
        Some((_, h)) => h,
        None => &url[..colon],
    };
    normalise_host(hostpart)
}

/// Strip a port and IPv6 brackets, and lowercase.
fn normalise_host(hostport: &str) -> Option<String> {
    let h = hostport.trim();
    let h = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        h.rsplit_once(':').map_or(h, |(host, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
                host
            } else {
                h
            }
        })
    };
    (!h.is_empty()).then(|| h.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_forms() {
        assert_eq!(host_of_url("git@github.com:o/r.git").as_deref(), Some("github.com"));
        assert_eq!(host_of_url("github.com:o/r.git").as_deref(), Some("github.com"));
        assert_eq!(host_of_url("ssh://git@host:2222/o/r").as_deref(), Some("host"));
        assert_eq!(host_of_url("https://GitHub.COM/o/r.git").as_deref(), Some("github.com"));
        assert_eq!(host_of_url("https://user:pw@host/o/r").as_deref(), Some("host"));
        assert_eq!(host_of_url("git://host/o/r").as_deref(), Some("host"));
        assert_eq!(host_of_url("ssh://git@[::1]:22/o/r").as_deref(), Some("::1"));
    }

    #[test]
    fn local_remotes_have_no_host() {
        assert_eq!(host_of_url("/tmp/fixtures/origin.git"), None);
        assert_eq!(host_of_url("../origin.git"), None);
        assert_eq!(host_of_url("./o.git"), None);
        assert_eq!(host_of_url("~/o.git"), None);
        assert_eq!(host_of_url("file:///tmp/o.git"), None);
        assert_eq!(host_of_url(""), None);
    }

    #[test]
    fn a_path_containing_a_colon_is_not_scp_syntax() {
        assert_eq!(host_of_url("/tmp/weird:dir/o.git"), None);
        assert_eq!(host_of_url("sub/dir:x/o.git"), None);
    }

    #[test]
    fn reads_remote_stanzas() {
        let cfg = r#"
[core]
	bare = false
[remote "origin"]
	url = git@github.com:o/r.git
	fetch = +refs/heads/*:refs/remotes/origin/*
[remote "upstream"]
	url = https://gitlab.example.com/o/r.git
[branch "main"]
	remote = origin
"#;
        let remotes = parse_remotes_str(cfg);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].host.as_deref(), Some("github.com"));
        assert_eq!(remotes[1].name, "upstream");
        assert_eq!(remotes[1].host.as_deref(), Some("gitlab.example.com"));
    }

    #[test]
    fn a_remote_with_no_url_is_still_a_remote() {
        let remotes = parse_remotes_str("[remote \"empty\"]\n\tfetch = +x:y\n");
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].name, "empty");
        assert_eq!(remotes[0].host, None);
    }

    #[test]
    fn a_local_remote_yields_a_remote_with_no_host() {
        let remotes = parse_remotes_str("[remote \"origin\"]\n\turl = /tmp/o.git\n");
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].host, None);
    }

    #[test]
    fn ignores_everything_else() {
        let remotes = parse_remotes_str(
            "# [remote \"commented\"]\n[include]\n\tpath = other\n[user]\n\tname = x\n",
        );
        assert!(remotes.is_empty());
    }
}

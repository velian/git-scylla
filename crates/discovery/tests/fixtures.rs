//! Discovery against the fixture set: the **exact** set of paths, and the
//! correct kind for each.
//!
//! Exactness is the point. A walker that finds everything it should plus a
//! `.git` directory or a bare repository that is really machinery is not
//! "mostly right"; it puts rows in the grid that the user cannot act on.

use git_scylla_discovery::{RepoFound, WalkOptions, Walker};
use git_scylla_testkit::FixtureSet;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn walk(root: &std::path::Path, nested: bool) -> BTreeMap<PathBuf, RepoFound> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_n, fatal) = Walker::new(vec![root.to_path_buf()])
        .options(WalkOptions { nested, max_depth: None })
        .walk(tx);
    assert!(fatal.is_empty(), "{fatal:?}");
    let mut out = BTreeMap::new();
    while let Ok(f) = rx.try_recv() {
        out.insert(f.path.strip_prefix(root).unwrap().to_path_buf(), f);
    }
    out
}

#[test]
fn discovers_exactly_the_fixture_set() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");

    let found = walk(&set.scan_root, false);
    let mut expected: Vec<PathBuf> = set
        .discoverable()
        .map(|f| f.path.strip_prefix(&set.scan_root).unwrap().to_path_buf())
        .collect();
    expected.sort();
    let actual: Vec<PathBuf> = found.keys().cloned().collect();
    assert_eq!(actual, expected);

    // Every kind reported must be the one the fixture declares.
    for f in set.discoverable() {
        let rel = f.path.strip_prefix(&set.scan_root).unwrap();
        assert_eq!(found[rel].kind, f.expect.kind, "kind for {}", f.name);
    }
}

#[test]
fn nested_adds_exactly_the_nested_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");

    let default = walk(&set.scan_root, false);
    let nested = walk(&set.scan_root, true);

    let added: Vec<_> = nested.keys().filter(|k| !default.contains_key(*k)).cloned().collect();
    let mut want: Vec<PathBuf> = set
        .fixtures
        .iter()
        .filter(|f| f.nested_only)
        .map(|f| f.path.strip_prefix(&set.scan_root).unwrap().to_path_buf())
        .collect();
    want.sort();
    assert_eq!(added, want, "--nested must add the nested fixtures and nothing else");
}

#[test]
fn a_submodule_is_not_discovered_by_default() {
    // A deliberate consequence of prune-on-match, asserted so that changing it
    // has to be a decision. A submodule is a repository inside another
    // repository's worktree; a bulk "pull everything" must not move submodules
    // off the commits their superprojects pin them to.
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let sub = set.get("submodule-sub").expect("submodule fixture");
    let rel = sub.path.strip_prefix(&set.scan_root).unwrap();

    assert!(!walk(&set.scan_root, false).contains_key(rel));
    let nested = walk(&set.scan_root, true);
    assert_eq!(
        nested[rel].kind,
        git_scylla_core::RepoKind::Submodule {
            parent: git_scylla_core::RepoId::new(set.scan_root.join("submodule-super")).unwrap()
        }
    );
}

#[test]
fn the_origins_used_as_remotes_are_outside_the_scan_root() {
    // The fixture set puts its bare "remotes" in a sibling directory on
    // purpose. If they leaked into repos/, the exactness assertion above would
    // be asserting the wrong set and would drift silently.
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    for f in &set.fixtures {
        assert!(f.path.starts_with(&set.scan_root), "{} escaped the scan root", f.name);
    }
    let origins = walk(&set.dir.join("origins"), false);
    assert!(!origins.is_empty(), "the fixture set should have local origins to fetch from");
}

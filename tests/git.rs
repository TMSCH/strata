use serde_json::{Value, json};
use std::{fs, os::unix::fs::symlink, path::Path, process::Command, thread};
use strata::{Append, Receipt, Store, git::verify_staged};
use tempfile::{TempDir, tempdir};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn repo() -> TempDir {
    let root = tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["config", "user.name", "Strata Test"]);
    git(
        root.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(root.path(), &["config", "commit.gpgsign", "false"]);
    git(root.path(), &["config", "core.hooksPath", "/dev/null"]);
    fs::write(root.path().join(".gitignore"), "events/.strata*\n").unwrap();
    root
}
fn request(id: &str) -> Append {
    Append {
        id: id.into(),
        kind: "food".into(),
        data: json!({"content":"steak"}),
    }
}
fn stage(repo: &Path) {
    git(repo, &["add", "-A"]);
}
fn commit(repo: &Path) {
    git(repo, &["commit", "-qm", "snapshot"]);
}
fn baseline() -> (TempDir, Receipt, Receipt) {
    let root = repo();
    let mut s = Store::open(root.path().join("events")).unwrap();
    let a = s.append(request("a")).unwrap();
    let b = s.append(request("b")).unwrap();
    stage(root.path());
    verify_staged(root.path(), "events", None, true).unwrap();
    commit(root.path());
    (root, a, b)
}

#[test]
fn initial_is_explicit_and_not_allowed_to_bypass_existing_baseline() {
    let root = repo();
    stage(root.path());
    assert!(verify_staged(root.path(), "events", None, false).is_err());
    assert_eq!(
        verify_staged(root.path(), "events", None, true)
            .unwrap()
            .verification
            .events,
        0
    );
    commit(root.path());
    assert!(verify_staged(root.path(), "events", None, true).is_err());
    assert!(verify_staged(root.path(), "events", Some("missing-ref"), false).is_err());
}

#[test]
fn staged_content_is_verified_instead_of_working_files() {
    let (root, _, b) = baseline();
    let path = root.path().join("events").join(b.file);
    let original = fs::read(&path).unwrap();
    fs::write(&path, b"corrupt working tree\n").unwrap();
    assert_eq!(
        verify_staged(root.path(), "events", None, false)
            .unwrap()
            .verification
            .events,
        2
    );
    stage(root.path());
    fs::write(&path, original).unwrap();
    assert!(verify_staged(root.path(), "events", None, false).is_err());
    stage(root.path());
    assert!(verify_staged(root.path(), "events", None, false).is_ok());
}

#[test]
fn baseline_rejects_tail_deletion_even_when_remaining_chain_is_valid() {
    let (root, _, b) = baseline();
    fs::remove_file(root.path().join("events").join(b.file)).unwrap();
    stage(root.path());
    assert_eq!(
        strata::verify(root.path().join("events"), None)
            .unwrap()
            .events,
        1
    );
    assert!(
        verify_staged(root.path(), "events", None, false)
            .unwrap_err()
            .to_string()
            .contains("baseline event missing")
    );
}

#[test]
fn compaction_overlap_and_every_partial_cleanup_preserve_baseline() {
    let (root, a, b) = baseline();
    let dir = root.path().join("events");
    let bytes = [
        fs::read(dir.join(&a.file)).unwrap(),
        fs::read(dir.join(&b.file)).unwrap(),
    ]
    .concat();
    let archive = format!("{}.jsonl", a.file.split('/').next().unwrap());
    fs::write(dir.join(archive), bytes).unwrap();
    for remove in [None, Some(a.file), Some(b.file)] {
        if let Some(name) = remove {
            fs::remove_file(dir.join(name)).unwrap();
        }
        stage(root.path());
        let result = verify_staged(root.path(), "events", None, false).unwrap();
        assert_eq!(result.verification.events, 2);
        assert_eq!(result.verification.baseline_events, Some(2));
    }
    commit(root.path());
    assert!(verify_staged(root.path(), "events", None, false).is_ok());
}

#[test]
fn git_can_stage_successfully_yet_verification_catches_compaction_race() {
    let (root, a, b) = baseline();
    let dir = root.path().join("events");
    let archive = format!("{}.jsonl", a.file.split('/').next().unwrap());
    let bytes = [
        fs::read(dir.join(&a.file)).unwrap(),
        fs::read(dir.join(&b.file)).unwrap(),
    ]
    .concat();
    fs::write(dir.join(archive), bytes).unwrap();
    fs::remove_file(dir.join(&a.file)).unwrap();
    fs::remove_file(dir.join(&b.file)).unwrap();
    // Model Git discovering names before the archive appeared, then observing
    // loose-file deletions: successful staging, but no events in the index.
    git(root.path(), &["add", "-u", "--", "events"]);
    assert!(verify_staged(root.path(), "events", None, false).is_err());
    stage(root.path());
    assert_eq!(
        verify_staged(root.path(), "events", None, false)
            .unwrap()
            .verification
            .events,
        2
    );
}

#[test]
fn missing_new_middle_fails_but_new_unstaged_tail_is_allowed() {
    let root = repo();
    let mut s = Store::open(root.path().join("events")).unwrap();
    let a = s.append(request("a")).unwrap();
    stage(root.path());
    commit(root.path());
    let b = s.append(request("b")).unwrap();
    let c = s.append(request("c")).unwrap();
    assert_eq!(
        verify_staged(root.path(), "events", None, false)
            .unwrap()
            .verification
            .head,
        Some(a.hash)
    );
    git(root.path(), &["add", "--", &format!("events/{}", c.file)]);
    assert!(verify_staged(root.path(), "events", None, false).is_err());
    git(root.path(), &["add", "--", &format!("events/{}", b.file)]);
    assert_eq!(
        verify_staged(root.path(), "events", None, false)
            .unwrap()
            .verification
            .events,
        3
    );
}

#[test]
fn returned_tree_stays_fixed_after_later_staging() {
    let (root, _, _) = baseline();
    let accepted = verify_staged(root.path(), "events", None, false).unwrap();
    let mut s = Store::open(root.path().join("events")).unwrap();
    s.append(request("c")).unwrap();
    stage(root.path());
    assert_ne!(git(root.path(), &["write-tree"]), accepted.tree);
    // The returned tree can be used by a trusted commit-tree coordinator.
    let commit_id = git(
        root.path(),
        &[
            "commit-tree",
            &accepted.tree,
            "-p",
            accepted.baseline_commit.as_deref().unwrap(),
            "-m",
            "verified",
        ],
    );
    assert_eq!(
        git(
            root.path(),
            &["rev-parse", &format!("{commit_id}^{{tree}}")]
        ),
        accepted.tree
    );
}

#[test]
fn rejects_unsafe_paths_symlink_and_tracked_private_files() {
    let (root, a, _) = baseline();
    for path in [
        "",
        "/events",
        "../events",
        "events/",
        "./events",
        ".git",
        "events//x",
    ] {
        assert!(verify_staged(root.path(), path, None, false).is_err());
    }
    git(root.path(), &["add", "-f", "events/.strata.lock"]);
    assert!(verify_staged(root.path(), "events", None, false).is_err());
    git(
        root.path(),
        &["reset", "-q", "HEAD", "--", "events/.strata.lock"],
    );
    let path = root.path().join("events").join(&a.file);
    fs::remove_file(&path).unwrap();
    symlink("/dev/null", &path).unwrap();
    stage(root.path());
    assert!(verify_staged(root.path(), "events", None, false).is_err());
}

#[test]
fn unmerged_index_is_rejected() {
    let (root, a, _) = baseline();
    let path = format!("events/{}", a.file);
    let object = git(root.path(), &["rev-parse", &format!("HEAD:{path}")]);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    write!(
        child.stdin.take().unwrap(),
        "0 {}\t{path}\n100644 {object} 1\t{path}\n100644 {object} 2\t{path}\n",
        "0".repeat(40)
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
    assert!(verify_staged(root.path(), "events", None, false).is_err());
}

#[test]
fn appends_can_run_during_staging_and_verification() {
    let root = repo();
    let mut store = Store::open(root.path().join("events")).unwrap();
    store.append(request("first")).unwrap();
    stage(root.path());
    commit(root.path());
    let writer = thread::spawn(move || {
        for n in 0..40 {
            store.append(request(&format!("new-{n}"))).unwrap();
        }
        store
    });
    for _ in 0..8 {
        stage(root.path());
        // A scan may omit a newly published predecessor and is then rejected.
        if let Ok(result) = verify_staged(root.path(), "events", None, false) {
            assert!((1..=41).contains(&result.verification.events));
            assert_eq!(result.verification.baseline_events, Some(1));
        }
    }
    let store = writer.join().unwrap();
    stage(root.path());
    let out = Command::new(env!("CARGO_BIN_EXE_strata"))
        .args(["verify", "--staged", "--repo"])
        .arg(root.path())
        .args(["--path", "events"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["events"], 41);
    assert_eq!(store.status().events, 41);
}

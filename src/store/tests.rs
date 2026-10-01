use super::*;
use crate::record::{MAX_RECORD, decode};
use serde_json::json;
use std::os::unix::fs::symlink;
use tempfile::tempdir;

fn request(id: &str) -> Append {
    Append {
        id: id.into(),
        kind: "client.food".into(),
        data: json!({"client":"alice", "content":"steak and fries"}),
    }
}
fn at(day: u8) -> OffsetDateTime {
    time::Date::from_calendar_date(2026, time::Month::September, day)
        .unwrap()
        .with_hms(12, 0, 0)
        .unwrap()
        .assume_utc()
}

#[test]
fn immutable_append_restart_retry_and_conflict() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    let original = fs::read(dir.path().join(&a.file)).unwrap();
    assert_eq!(a.file, format!("2026-09-29/{}.json", a.hash));
    let b = s.append_at(request("b"), at(29)).unwrap();
    assert_eq!(fs::read(dir.path().join(&a.file)).unwrap(), original);
    assert_eq!(
        decode(&fs::read(dir.path().join(&b.file)).unwrap())
            .unwrap()
            .0
            .previous,
        Some(a.hash.clone())
    );
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    assert_eq!(s.append_at(request("a"), at(30)).unwrap(), a);
    let mut different = request("a");
    different.data = json!({"different":true});
    assert!(s.append_at(different, at(30)).is_err());
}

#[test]
fn rollover_compacts_exact_bytes_and_retry_location() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    let b = s.append_at(request("b"), at(29)).unwrap();
    let bytes = [
        fs::read(dir.path().join(&a.file)).unwrap(),
        fs::read(dir.path().join(&b.file)).unwrap(),
    ]
    .concat();
    s.compact_before("2026-09-29").unwrap();
    assert!(dir.path().join(&a.file).exists());
    let c = s.append_at(request("c"), at(30)).unwrap();
    assert!(!dir.path().join("2026-09-29").exists());
    assert_eq!(
        fs::read(dir.path().join("2026-09-29.jsonl")).unwrap(),
        bytes
    );
    let retry = s.append_at(request("a"), at(30)).unwrap();
    assert_eq!(retry.hash, a.hash);
    assert_eq!(retry.file, "2026-09-29.jsonl");
    assert_eq!(
        decode(&fs::read(dir.path().join(c.file)).unwrap())
            .unwrap()
            .0
            .previous,
        Some(b.hash)
    );
    s.compact_before("2026-09-30").unwrap();
    assert_eq!(
        fs::read(dir.path().join("2026-09-29.jsonl")).unwrap(),
        bytes
    );
    drop(s);
    assert_eq!(verify(dir.path(), None).unwrap().events, 3);
}

#[test]
fn restart_at_each_compaction_boundary() {
    // Archive absent, published with all sources, and every partial cleanup.
    for removed in 0..=3 {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let receipts: Vec<_> = (0..3)
            .map(|i| s.append_at(request(&format!("e{i}")), at(29)).unwrap())
            .collect();
        let bytes: Vec<_> = receipts
            .iter()
            .flat_map(|r| fs::read(dir.path().join(&r.file)).unwrap())
            .collect();
        s.publish("2026-09-29.jsonl", &bytes).unwrap();
        for r in receipts.iter().take(removed) {
            fs::remove_file(dir.path().join(&r.file)).unwrap();
        }
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(s.status().events, 3);
        s.compact_before("2026-09-30").unwrap();
        assert_eq!(
            s.append_at(request("e0"), at(30)).unwrap().hash,
            receipts[0].hash
        );
        assert_eq!(
            fs::read(dir.path().join("2026-09-29.jsonl")).unwrap(),
            bytes
        );
        assert_eq!(files(dir.path()).unwrap(), vec!["2026-09-29.jsonl"]);
    }
}

#[test]
fn legacy_daily_archive_stays_immutable_with_new_same_day_events() {
    let dir = tempdir().unwrap();
    let fixture = include_bytes!("../../tests/fixtures/2026-09-30.jsonl");
    fs::write(dir.path().join("2026-09-30.jsonl"), fixture).unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.append_at(request("new"), at(30)).unwrap();
    s.compact_before("2026-10-01").unwrap();
    assert_eq!(
        fs::read(dir.path().join("2026-09-30.jsonl")).unwrap(),
        fixture
    );
    assert_eq!(files(dir.path()).unwrap().len(), 2);
    assert!(
        files(dir.path())
            .unwrap()
            .iter()
            .any(|n| n.starts_with("2026-09-30--"))
    );
    drop(s);
    assert_eq!(verify(dir.path(), None).unwrap().events, 2);
}

#[test]
fn publication_never_overwrites_and_failed_write_poisoning() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let r = s.append_at(request("a"), at(30)).unwrap();
    let original = fs::read(dir.path().join(&r.file)).unwrap();
    assert!(s.publish(&r.file, b"replacement").is_err());
    assert_eq!(fs::read(dir.path().join(&r.file)).unwrap(), original);
    fs::remove_dir(dir.path().join(".strata-tmp")).unwrap();
    fs::write(dir.path().join(".strata-tmp"), b"block publication").unwrap();
    assert!(s.append_at(request("b"), at(30)).is_err());
    fs::remove_file(dir.path().join(".strata-tmp")).unwrap();
    assert!(
        s.append_at(request("b"), at(30))
            .unwrap_err()
            .to_string()
            .contains("storage error")
    );
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    assert_eq!(s.append_at(request("b"), at(30)).unwrap().sequence, 2);
}

#[test]
fn unpublished_torn_files_are_ignored_but_public_torn_files_fail() {
    let source = tempdir().unwrap();
    let mut s = Store::open(source.path()).unwrap();
    let r = s.append_at(request("a"), at(30)).unwrap();
    let bytes = fs::read(source.path().join(&r.file)).unwrap();
    drop(s);
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".strata-tmp")).unwrap();
    fs::create_dir(dir.path().join("2026-09-30")).unwrap();
    for cut in 0..bytes.len() {
        fs::write(dir.path().join(".strata-tmp/unpublished"), &bytes[..cut]).unwrap();
        assert_eq!(Store::open(dir.path()).unwrap().status().events, 0);
        fs::write(dir.path().join(&r.file), &bytes[..cut]).unwrap();
        assert!(Store::open(dir.path()).is_err(), "cut {cut}");
        assert_eq!(fs::read(dir.path().join(&r.file)).unwrap(), bytes[..cut]);
        fs::remove_file(dir.path().join(&r.file)).unwrap();
    }
    fs::write(dir.path().join(&r.file), &bytes).unwrap();
    assert_eq!(
        Store::open(dir.path())
            .unwrap()
            .append_at(request("a"), at(30))
            .unwrap(),
        r
    );
}

#[test]
fn rejects_edits_missing_middle_wrong_names_and_conflicting_sequences() {
    for mutation in 0..5 {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let a = s.append_at(request("a"), at(30)).unwrap();
        let b = s.append_at(request("b"), at(30)).unwrap();
        s.append_at(request("c"), at(30)).unwrap();
        drop(s);
        let path = dir.path().join(&b.file);
        let bytes = fs::read(&path).unwrap();
        match mutation {
            0 => fs::write(
                &path,
                String::from_utf8(bytes).unwrap().replace("steak", "salad"),
            )
            .unwrap(),
            1 => fs::remove_file(&path).unwrap(),
            2 => {
                fs::rename(
                    &path,
                    dir.path()
                        .join(format!("2026-09-30/{}.json", "0".repeat(64))),
                )
                .unwrap();
            }
            3 => {
                let mut bytes = bytes;
                bytes.insert(1, b' ');
                fs::write(&path, bytes).unwrap();
            }
            _ => {
                let (mut event, _) = decode(&fs::read(dir.path().join(a.file)).unwrap()).unwrap();
                event.id = "conflict".into();
                fs::write(
                    dir.path().join("2026-09-30.jsonl"),
                    encode(&event).unwrap().0,
                )
                .unwrap();
            }
        }
        assert!(Store::open(dir.path()).is_err(), "mutation {mutation}");
    }
}

#[test]
fn baseline_survives_compaction_but_detects_deleted_tail_and_rewrites() {
    let baseline = tempdir().unwrap();
    let current = tempdir().unwrap();
    let mut s = Store::open(baseline.path()).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    let first = fs::read(baseline.path().join(&a.file)).unwrap();
    let b = s.append_at(request("b"), at(29)).unwrap();
    let all = [
        first.clone(),
        fs::read(baseline.path().join(b.file)).unwrap(),
    ]
    .concat();
    drop(s);
    fs::write(current.path().join("2026-09-29.jsonl"), &all).unwrap();
    assert_eq!(
        verify(current.path(), Some(baseline.path()))
            .unwrap()
            .baseline_events,
        Some(2)
    );
    fs::write(current.path().join("2026-09-29.jsonl"), first).unwrap();
    assert!(verify(current.path(), None).is_ok());
    assert!(verify(current.path(), Some(baseline.path())).is_err());
    let (mut event, _) = decode(&fs::read(baseline.path().join(a.file)).unwrap()).unwrap();
    event.data = json!({"rewritten":true});
    fs::write(
        current.path().join("2026-09-29.jsonl"),
        encode(&event).unwrap().0,
    )
    .unwrap();
    assert!(verify(current.path(), None).is_ok());
    assert!(verify(current.path(), Some(baseline.path())).is_err());
}

#[test]
fn rejects_backwards_clock_without_poisoning() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.append_at(request("a"), at(30)).unwrap();
    assert!(s.append_at(request("b"), at(29)).is_err());
    assert_eq!(s.append_at(request("b"), at(30)).unwrap().sequence, 2);
}

#[test]
fn exclusive_writer_and_offline_verifier() {
    let dir = tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert!(verify(dir.path(), None).is_err());
    drop(s);
    assert!(Store::open(dir.path()).is_ok());
}

#[test]
fn symlinks_hardlinks_and_nonfiles_are_rejected() {
    let root = tempdir().unwrap();
    let external = root.path().join("outside");
    fs::write(&external, b"untouched").unwrap();
    for hard in [false, true] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("2026-09-30.jsonl");
        if hard {
            fs::hard_link(&external, &path).unwrap();
        } else {
            symlink(&external, &path).unwrap();
        }
        assert!(Store::open(dir.path()).is_err());
        assert_eq!(fs::read(&external).unwrap(), b"untouched");
    }
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("2026-09-30.jsonl")).unwrap();
    assert!(Store::open(dir.path()).is_err());
    let alias = root.path().join("alias");
    symlink(dir.path(), &alias).unwrap();
    assert!(Store::open(alias).is_err());
}

#[test]
fn bad_filename_is_an_error_not_a_panic() {
    let dir = tempdir().unwrap();
    for name in ["not-a-date.jsonl", "€€€a.jsonl"] {
        fs::write(dir.path().join(name), b"").unwrap();
    }
    assert!(Store::open(dir.path()).is_err());
}

#[test]
fn rejects_invalid_and_oversized_requests() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let mut cases = vec![];
    let mut a = request("");
    cases.push(a.clone());
    a.id = "../escape".into();
    cases.push(a);
    let mut a = request("a");
    a.kind = "bad\ntype".into();
    cases.push(a);
    let mut a = request("a");
    a.data = json!([1, 2]);
    cases.push(a);
    let mut a = request("a");
    a.data = json!({"large":"x".repeat(MAX_RECORD)});
    cases.push(a);
    for request in cases {
        assert!(s.append_at(request, at(30)).is_err());
    }
    assert_eq!(s.status().events, 0);
}

#[test]
fn unicode_nested_data_and_float_roundtrip() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let mut a = request("a");
    a.data = json!({"text":"游泳 🏊\nnext line", "nested":{"a":[null,true,0.1,1.2345678901234567]}, "max":u64::MAX});
    s.append_at(a, at(30)).unwrap();
    drop(s);
    assert_eq!(verify(dir.path(), None).unwrap().events, 1);
}

#[test]
fn empty_store_and_missing_baseline() {
    let dir = tempdir().unwrap();
    assert_eq!(verify(dir.path(), None).unwrap().events, 0);
    assert!(verify(dir.path(), Some(&dir.path().join("missing"))).is_err());
}

#[test]
fn creates_only_leaf_directory_and_rejects_missing_parent() {
    let root = tempdir().unwrap();
    let leaf = root.path().join("events");
    assert!(Store::open(&leaf).is_ok());
    assert!(leaf.is_dir());
    assert!(Store::open(root.path().join("missing/events")).is_err());
}

#[test]
fn archive_order_duplicates_and_supplement_hash_are_checked() {
    let source = tempdir().unwrap();
    let mut store = Store::open(source.path()).unwrap();
    let a = store.append_at(request("a"), at(30)).unwrap();
    let b = store.append_at(request("b"), at(30)).unwrap();
    let first = fs::read(source.path().join(a.file)).unwrap();
    let second = fs::read(source.path().join(b.file)).unwrap();
    for bytes in [
        [second.clone(), first.clone()].concat(),
        [first.clone(), first.clone()].concat(),
    ] {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("2026-09-30.jsonl"), bytes).unwrap();
        assert!(Store::open(dir.path()).is_err());
    }
    let dir = tempdir().unwrap();
    fs::write(
        dir.path()
            .join(format!("2026-09-30--{}.jsonl", "0".repeat(64))),
        first,
    )
    .unwrap();
    assert!(Store::open(dir.path()).is_err());
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn cleanup_waits_for_exact_archive_in_head_not_index_or_other_branch() {
    let root = tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    let dir = root.path().join("events");
    let mut s = Store::open(&dir).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    s.compact_before("2026-09-30").unwrap();
    let archive = dir.join("2026-09-29.jsonl");
    let original = fs::read(&archive).unwrap();
    assert!(dir.join(&a.file).exists());
    git(root.path(), &["add", "events/2026-09-29.jsonl"]);
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists(), "staged is not committed");
    // Commit wrong bytes, then restore the valid working archive.
    fs::write(&archive, b"wrong bytes\n").unwrap();
    git(root.path(), &["add", "events/2026-09-29.jsonl"]);
    git(root.path(), &["commit", "-qm", "wrong archive"]);
    fs::write(&archive, &original).unwrap();
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists());
    let old = git(root.path(), &["rev-parse", "HEAD"]);
    git(root.path(), &["add", "events/2026-09-29.jsonl"]);
    git(root.path(), &["commit", "-qm", "correct archive"]);
    git(root.path(), &["branch", "saved"]);
    git(root.path(), &["reset", "--soft", &old]);
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists(), "other branch is not HEAD");
    git(root.path(), &["commit", "-qm", "correct current archive"]);
    drop(s);
    let mut s = Store::open(&dir).unwrap();
    s.compact_before("2026-09-30").unwrap();
    assert!(!dir.join(&a.file).exists());
    assert_eq!(fs::read(archive).unwrap(), original);
    assert_eq!(s.append_at(request("a"), at(30)).unwrap().hash, a.hash);
    git(root.path(), &["add", "-u"]);
    assert_eq!(git(root.path(), &["diff", "--cached", "--name-only"]), "");
}

#[test]
fn cleanup_detects_git_initialized_after_store_open_and_broken_git_marker() {
    let root = tempdir().unwrap();
    let dir = root.path().join("events");
    let mut s = Store::open(&dir).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    fs::write(
        root.path().join(".git"),
        b"gitdir: /nonexistent-strata-git\n",
    )
    .unwrap();
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists());
    assert_eq!(s.append_at(request("b"), at(30)).unwrap().sequence, 2);
    fs::remove_file(root.path().join(".git")).unwrap();
    git(root.path(), &["init", "-q"]);
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists());
    git(root.path(), &["add", "events/2026-09-29.jsonl"]);
    git(root.path(), &["commit", "-qm", "archive"]);
    s.compact_before("2026-09-30").unwrap();
    assert!(!dir.join(&a.file).exists());
}

#[test]
fn linked_worktree_and_literal_directory_names_are_supported() {
    let root = tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["commit", "--allow-empty", "-qm", "initial"]);
    let worktree = root.path().join("worktree");
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "-qb",
            "other",
            worktree.to_str().unwrap(),
        ],
    );
    let dir = worktree.join("events [personal]");
    let mut s = Store::open(&dir).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists());
    git(
        &worktree,
        &["add", "--", "events [personal]/2026-09-29.jsonl"],
    );
    git(&worktree, &["commit", "-qm", "archive"]);
    s.compact_before("2026-09-30").unwrap();
    assert!(!dir.join(&a.file).exists());
}

#[test]
fn every_supplement_must_be_committed_before_day_cleanup() {
    let root = tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    let dir = root.path().join("events");
    let mut s = Store::open(&dir).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    s.compact_before("2026-09-30").unwrap();
    let b = s.append_at(request("b"), at(29)).unwrap();
    s.compact_before("2026-09-30").unwrap();
    git(root.path(), &["add", "events/2026-09-29.jsonl"]);
    git(root.path(), &["commit", "-qm", "primary archive"]);
    s.compact_before("2026-09-30").unwrap();
    assert!(dir.join(&a.file).exists() && dir.join(&b.file).exists());
    let supplement = files(&dir)
        .unwrap()
        .into_iter()
        .find(|n| n.contains("--"))
        .unwrap();
    git(root.path(), &["add", &format!("events/{supplement}")]);
    git(root.path(), &["commit", "-qm", "supplement"]);
    s.compact_before("2026-09-30").unwrap();
    assert!(!dir.join("2026-09-29").exists());
    assert_eq!(s.status().events, 2);
}

#[test]
fn ordinary_commits_and_clone_preserve_events_through_cleanup() {
    let root = tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    fs::write(root.path().join(".gitignore"), "events/.strata*\n").unwrap();
    let dir = root.path().join("events");
    let mut s = Store::open(&dir).unwrap();
    let receipt = s.append_at(request("a"), at(29)).unwrap();
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-qm", "loose event"]);
    s.compact_before("2026-09-30").unwrap();
    // A commit that stages only known paths must retain the loose event.
    git(root.path(), &["add", "-u"]);
    assert_eq!(git(root.path(), &["diff", "--cached", "--name-only"]), "");
    assert!(dir.join(&receipt.file).exists());
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-qm", "archive and loose event"]);
    s.compact_before("2026-09-30").unwrap();
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-qm", "cleanup"]);
    let clone = root.path().join("restored");
    git(
        root.path(),
        &["clone", "-q", "--no-local", ".", clone.to_str().unwrap()],
    );
    let recovered = Store::open(clone.join("events")).unwrap();
    assert_eq!(recovered.status().head, Some(receipt.hash));
    assert_eq!(recovered.status().events, 1);
}

#[test]
fn closing_store_releases_lock_even_if_descriptor_was_inherited() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    // Like a descriptor inherited by fork, this shares the open description.
    let inherited = store._lock.try_clone().unwrap();
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    drop(inherited);
    assert!(Store::open(dir.path()).is_err());
    drop(reopened);
    assert!(Store::open(dir.path()).is_ok());
}

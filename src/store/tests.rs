use super::*;
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
fn logs(dir: &Path) -> Vec<u8> {
    files(dir)
        .unwrap()
        .iter()
        .flat_map(|p| fs::read(p).unwrap())
        .collect()
}

#[test]
fn append_restart_retry_and_conflicting_id() {
    let dir = tempdir().unwrap();
    let first;
    {
        let mut s = Store::open(dir.path()).unwrap();
        first = s.append_at(request("one"), at(29)).unwrap();
        assert_eq!(s.append_at(request("one"), at(30)).unwrap(), first);
    }
    let mut s = Store::open(dir.path()).unwrap();
    assert_eq!(s.append_at(request("one"), at(30)).unwrap(), first);
    let mut conflict = request("one");
    conflict.data = json!({"different":true});
    assert!(s.append_at(conflict, at(30)).is_err());
    assert_eq!(s.append_at(request("two"), at(30)).unwrap().sequence, 2);
    drop(s);
    assert_eq!(verify(dir.path(), None).unwrap().events, 2);
}

#[test]
fn rollover_links_files_and_leaves_yesterday_unchanged() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let a = s.append_at(request("a"), at(29)).unwrap();
    let yesterday = logs(dir.path());
    let b = s.append_at(request("b"), at(30)).unwrap();
    assert_eq!(fs::read(dir.path().join(a.file)).unwrap(), yesterday);
    let (event, _) = decode(&fs::read(dir.path().join(b.file)).unwrap()).unwrap();
    assert_eq!(event.previous, Some(a.hash));
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
fn rejects_edits_deletions_reordering_and_bad_encoding() {
    for mutation in 0..5 {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        for id in ["a", "b", "c"] {
            s.append_at(request(id), at(30)).unwrap();
        }
        drop(s);
        let path = dir.path().join("2026-09-30.jsonl");
        let original = fs::read_to_string(&path).unwrap();
        let mut lines: Vec<_> = original.lines().map(str::to_owned).collect();
        match mutation {
            0 => lines[0] = lines[0].replace("steak", "salad"),
            1 => {
                lines.remove(1);
            }
            2 => lines.swap(0, 1),
            3 => lines[0].insert(1, ' '),
            _ => lines.push(lines[0].clone()),
        }
        fs::write(path, lines.join("\n") + "\n").unwrap();
        assert!(
            Store::open_with_recovery(dir.path(), true).is_err(),
            "mutation {mutation}"
        );
    }
}

#[test]
fn detects_tail_deletion_with_baseline() {
    let baseline = tempdir().unwrap();
    let current = tempdir().unwrap();
    let mut s = Store::open(baseline.path()).unwrap();
    s.append_at(request("a"), at(30)).unwrap();
    let first = logs(baseline.path());
    s.append_at(request("b"), at(30)).unwrap();
    drop(s);
    fs::write(current.path().join("2026-09-30.jsonl"), first).unwrap();
    assert!(verify(current.path(), None).is_ok());
    assert!(verify(current.path(), Some(baseline.path())).is_err());
}

#[test]
fn baseline_accepts_extension_and_rejects_rewritten_history() {
    let baseline = tempdir().unwrap();
    let current = tempdir().unwrap();
    let mut s = Store::open(baseline.path()).unwrap();
    s.append_at(request("a"), at(29)).unwrap();
    drop(s);
    fs::copy(
        baseline.path().join("2026-09-29.jsonl"),
        current.path().join("2026-09-29.jsonl"),
    )
    .unwrap();
    let mut s = Store::open(current.path()).unwrap();
    s.append_at(request("b"), at(30)).unwrap();
    drop(s);
    assert_eq!(
        verify(current.path(), Some(baseline.path()))
            .unwrap()
            .baseline_events,
        Some(1)
    );
    let rewritten = tempdir().unwrap();
    let mut s = Store::open(rewritten.path()).unwrap();
    let mut a = request("a");
    a.data = json!({"changed":true});
    s.append_at(a, at(29)).unwrap();
    drop(s);
    assert!(verify(rewritten.path(), Some(baseline.path())).is_err());
}

#[test]
fn recovery_at_every_possible_torn_write_boundary() {
    let source = tempdir().unwrap();
    let mut s = Store::open(source.path()).unwrap();
    s.append_at(request("a"), at(30)).unwrap();
    let first = logs(source.path());
    let receipt = s.append_at(request("b"), at(30)).unwrap();
    let all = logs(source.path());
    let second = &all[first.len()..];
    drop(s);
    for cut in 1..second.len() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("2026-09-30.jsonl");
        let mut torn = first.clone();
        torn.extend_from_slice(&second[..cut]);
        fs::write(&path, &torn).unwrap();
        assert!(Store::open(dir.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), torn);
        let mut recovered = Store::open_with_recovery(dir.path(), true).unwrap();
        assert_eq!(recovered.status().events, 1);
        assert_eq!(fs::read(&path).unwrap(), first);
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "bin"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(&backups[0]).unwrap(), second[..cut]);
        assert_eq!(recovered.append_at(request("b"), at(30)).unwrap(), receipt);
    }
}

#[test]
fn recovery_never_truncates_an_older_day() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.append_at(request("a"), at(29)).unwrap();
    s.append_at(request("b"), at(30)).unwrap();
    drop(s);
    let path = dir.path().join("2026-09-29.jsonl");
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    assert!(Store::open_with_recovery(dir.path(), true).is_err());
}

#[test]
fn completed_unacknowledged_record_survives_reopen() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let receipt = s.append_at(request("a"), at(30)).unwrap();
    drop(s);
    let mut s = Store::open_with_recovery(dir.path(), true).unwrap();
    assert_eq!(s.append_at(request("a"), at(30)).unwrap(), receipt);
}

#[test]
fn malformed_completed_line_is_never_repaired() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("2026-09-30.jsonl");
    fs::write(&path, b"bad json\n").unwrap();
    assert!(Store::open_with_recovery(dir.path(), true).is_err());
    assert_eq!(fs::read(path).unwrap(), b"bad json\n");
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
fn storage_failure_poisoning_prevents_further_appends() {
    let dir = tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let path = dir.path().join("2026-09-30.jsonl");
    fs::create_dir(&path).unwrap();
    assert!(s.append_at(request("a"), at(30)).is_err());
    fs::remove_dir(path).unwrap();
    assert!(
        s.append_at(request("a"), at(30))
            .unwrap_err()
            .to_string()
            .contains("storage error")
    );
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

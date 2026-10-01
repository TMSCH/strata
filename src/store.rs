use crate::{
    layout::{self, Snapshot},
    record::{Append, Event, Receipt, day, encode},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufReader, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

struct Seen {
    fingerprint: String,
    receipt: Receipt,
}

/// Holds the exclusive writer lock. Staging may run concurrently; checkout,
/// reset, restore, or any other external mutation requires stopping the writer.
pub struct Store {
    dir: PathBuf,
    _lock: File,
    seen: HashMap<String, Seen>,
    sequence: u64,
    head: Option<String>,
    last_time: Option<OffsetDateTime>,
    poisoned: bool,
}

#[derive(Debug, Serialize)]
pub struct Verification {
    pub events: u64,
    pub head: Option<String>,
    pub baseline_events: Option<u64>,
}

fn open_regular(path: &Path, write: bool, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
        "expected regular, non-hard-linked file: {}",
        path.display()
    );
    Ok(file)
}

fn real_dir(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "expected real directory: {}",
        path.display()
    );
    Ok(())
}

fn create_dir(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => real_dir(path)?,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn files(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF-8 store filename"))?;
        if name == ".git"
            || name == ".strata.lock"
            || name.starts_with(".strata-recovery-")
            || layout::auxiliary(&name)
        {
            continue;
        }
        if name == ".strata-tmp" {
            real_dir(&entry.path())?;
            continue;
        }
        if name.ends_with(".jsonl") {
            layout::location(&name)?;
            names.push(name);
        } else {
            layout::date(&name)?;
            real_dir(&entry.path())?;
            for child in fs::read_dir(entry.path())? {
                let child = child?;
                let file = child
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF-8 event filename"))?;
                let relative = format!("{name}/{file}");
                layout::location(&relative)?;
                names.push(relative);
            }
        }
    }
    names.sort();
    Ok(names)
}

fn snapshot(dir: &Path) -> Result<Snapshot> {
    let mut result = Snapshot::default();
    for name in files(dir)? {
        let file = open_regular(&dir.join(&name), false, false)?;
        result.add(&name, BufReader::new(&file))?;
        // Recover a fully published but unacknowledged record durably.
        file.sync_all()?;
    }
    result.validate()?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(dir)?.sync_all()?;
    Ok(result)
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        create_dir(dir).context("create store (its parent must already exist)")?;
        let lock = open_regular(&dir.join(".strata.lock"), true, true)?;
        lock.try_lock()
            .context("store is already in use; use verify --staged for live Git snapshots")?;
        let parent = dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        let data = snapshot(dir)?;
        let mut store = Self {
            dir: dir.to_owned(),
            _lock: lock,
            seen: HashMap::new(),
            sequence: 0,
            head: None,
            last_time: None,
            poisoned: false,
        };
        store.load(&data)?;
        Ok(store)
    }

    fn load(&mut self, data: &Snapshot) -> Result<()> {
        self.seen.clear();
        for record in data.records.values() {
            self.seen.insert(
                record.event.id.clone(),
                Seen {
                    fingerprint: record.request().fingerprint()?,
                    receipt: record.receipt(),
                },
            );
        }
        let status = data.status();
        self.sequence = status.events;
        self.head = status.head;
        self.last_time = data
            .records
            .last_key_value()
            .map(|(_, r)| OffsetDateTime::parse(&r.event.recorded_at, &Rfc3339))
            .transpose()?;
        Ok(())
    }

    pub fn append(&mut self, request: Append) -> Result<Receipt> {
        self.append_at(request, OffsetDateTime::now_utc())
    }

    fn append_at(&mut self, request: Append, now: OffsetDateTime) -> Result<Receipt> {
        ensure!(
            !self.poisoned,
            "writer stopped after a storage error; restart and verify before retrying"
        );
        request.validate()?;
        let fingerprint = request.fingerprint()?;
        if let Some(seen) = self.seen.get(&request.id) {
            ensure!(
                seen.fingerprint == fingerprint,
                "id already used for different content"
            );
            return Ok(seen.receipt.clone());
        }
        let now = now.to_offset(time::UtcOffset::UTC);
        ensure!(
            (0..=9999).contains(&now.year()),
            "clock is outside the supported year range"
        );
        ensure!(
            self.last_time.is_none_or(|last| now >= last),
            "system clock moved backwards; wait for it to catch up"
        );
        if self.last_time.is_some_and(|last| last.date() < now.date()) {
            self.compact_before(&day(now)[..10])?;
        }
        let event = Event {
            version: 1,
            sequence: self.sequence.checked_add(1).context("sequence exhausted")?,
            id: request.id.clone(),
            recorded_at: now.format(&Rfc3339)?,
            previous: self.head.clone(),
            kind: request.kind,
            data: request.data,
        };
        let (bytes, hash) = encode(&event)?;
        let folder = &day(now)[..10];
        let filename = format!("{folder}/{hash}.json");
        let receipt = Receipt {
            id: request.id.clone(),
            sequence: event.sequence,
            hash: hash.clone(),
            file: filename.clone(),
        };
        self.poisoned = true;
        create_dir(&self.dir.join(folder))?;
        File::open(&self.dir)?.sync_all()?;
        self.publish(&filename, &bytes)?;
        self.sequence = event.sequence;
        self.head = Some(hash);
        self.last_time = Some(now);
        self.seen.insert(
            request.id,
            Seen {
                fingerprint,
                receipt: receipt.clone(),
            },
        );
        self.poisoned = false;
        Ok(receipt)
    }

    /// Publish whole bytes atomically without replacing an existing path.
    fn publish(&self, filename: &str, bytes: &[u8]) -> Result<()> {
        let temporary = self.dir.join(".strata-tmp");
        create_dir(&temporary)?;
        let mut file = tempfile::NamedTempFile::new_in(&temporary)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        let target = self.dir.join(filename);
        // Do not fall back to a hard link: a crash between link/unlink would
        // conflict with the non-hard-linked-file invariant on recovery.
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            file.path(),
            rustix::fs::CWD,
            &target,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .context("atomic no-replace publication (local filesystem required)")?;
        File::open(target.parent().unwrap())?.sync_all()?;
        File::open(&temporary)?.sync_all()?;
        Ok(())
    }

    /// Archive completed UTC days. Safe to retry after any interrupted step.
    /// Library users can call this periodically; the daemon does so automatically.
    pub fn compact(&mut self) -> Result<()> {
        self.compact_before(&day(OffsetDateTime::now_utc())[..10])
    }

    fn compact_before(&mut self, today: &str) -> Result<()> {
        ensure!(
            !self.poisoned,
            "writer stopped after a storage error; restart before compaction"
        );
        let names = files(&self.dir)?;
        let days: std::collections::BTreeSet<_> = names
            .iter()
            .filter_map(|name| name.split_once('/').map(|(d, _)| d).filter(|d| *d < today))
            .collect();
        if days.is_empty() {
            return Ok(());
        }
        self.poisoned = true;
        let data = snapshot(&self.dir)?;
        for date in days {
            let loose: Vec<_> = names
                .iter()
                .filter(|n| n.starts_with(&format!("{date}/")))
                .collect();
            // Records already covered by a published archive only need cleanup.
            let uncovered: Vec<_> = data
                .records
                .values()
                .filter(|r| r.file.starts_with(&format!("{date}/")))
                .collect();
            if !uncovered.is_empty() {
                let bytes: Vec<_> = uncovered
                    .iter()
                    .flat_map(|r| r.bytes.iter().copied())
                    .collect();
                let primary = format!("{date}.jsonl");
                let filename = if self.dir.join(&primary).try_exists()? {
                    format!("{date}--{:x}.jsonl", Sha256::digest(&bytes))
                } else {
                    primary
                };
                self.publish(&filename, &bytes)?;
            }
            // Every archive for this day must be present byte-for-byte in HEAD.
            // Staging alone is insufficient. Failure only delays cleanup.
            let archives: Vec<_> = files(&self.dir)?
                .into_iter()
                .filter(|name| !name.contains('/') && name.starts_with(date))
                .collect();
            match crate::git::archives_committed(&self.dir, &archives) {
                Ok(true) => (),
                Ok(false) => continue,
                Err(error) => {
                    eprintln!("strata: cleanup deferred: {error:#}");
                    continue;
                }
            }
            // Archives are synchronized before any source is removed.
            for name in loose {
                fs::remove_file(self.dir.join(name))?;
            }
            File::open(self.dir.join(date))?.sync_all()?;
            fs::remove_dir(self.dir.join(date))?;
            File::open(&self.dir)?.sync_all()?;
        }
        self.load(&snapshot(&self.dir)?)?;
        self.poisoned = false;
        Ok(())
    }

    pub fn status(&self) -> Verification {
        Verification {
            events: self.sequence,
            head: self.head.clone(),
            baseline_events: None,
        }
    }
}

/// Offline verification. Baseline comparison ignores physical file placement.
pub fn verify(dir: impl AsRef<Path>, baseline: Option<&Path>) -> Result<Verification> {
    ensure!(dir.as_ref().is_dir(), "store does not exist");
    let store = Store::open(dir)?;
    let mut result = store.status();
    if let Some(baseline) = baseline {
        ensure!(baseline.is_dir(), "baseline does not exist");
        let previous = Store::open(baseline)?;
        for (id, old) in &previous.seen {
            let current = store
                .seen
                .get(id)
                .with_context(|| format!("baseline event missing: {id}"))?;
            ensure!(
                current.receipt.hash == old.receipt.hash
                    && current.receipt.sequence == old.receipt.sequence,
                "baseline event changed: {id}"
            );
        }
        result.baseline_events = Some(previous.sequence);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

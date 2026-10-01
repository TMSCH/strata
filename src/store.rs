use crate::record::{Append, Event, MAX_RECORD, Receipt, day, decode, encode};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

struct Seen {
    fingerprint: String,
    receipt: Receipt,
}

/// Holds the exclusive writer lock until dropped. External file changes while
/// open are unsupported: stop the writer before Git checkout/restore operations.
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
        file.metadata()?.is_file(),
        "not a regular file: {}",
        path.display()
    );
    ensure!(
        file.metadata()?.nlink() == 1,
        "hard-linked file rejected: {}",
        path.display()
    );
    Ok(file)
}

fn lock_dir(dir: &Path) -> Result<File> {
    ensure!(
        fs::symlink_metadata(dir)?.file_type().is_dir(),
        "store must be a real directory, not a symlink"
    );
    let lock = open_regular(&dir.join(".strata.lock"), true, true)?;
    lock.try_lock().context(
        "store is already in use (stop the daemon before verification or Git operations)",
    )?;
    Ok(lock)
}

fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".jsonl") {
            ensure!(
                name.is_ascii() && name.len() == 16,
                "unexpected JSONL filename: {name}"
            );
            let date = time::Date::parse(
                &name[..10],
                time::macros::format_description!("[year]-[month]-[day]"),
            )?;
            ensure!(date.to_string() == name[..10], "invalid daily filename");
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_recovery(dir, false)
    }

    /// Recovery is explicit: only an unterminated suffix in the last daily file
    /// can be quarantined. All preceding complete records must verify first.
    pub fn open_with_recovery(dir: impl AsRef<Path>, recover_tail: bool) -> Result<Self> {
        let dir = dir.as_ref();
        if !dir.exists() {
            DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .context("create store (its parent directory must already exist)")?;
        }
        let lock = lock_dir(dir)?;
        let parent = dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        let mut store = Self {
            dir: dir.to_owned(),
            _lock: lock,
            seen: HashMap::new(),
            sequence: 0,
            head: None,
            last_time: None,
            poisoned: false,
        };
        store.scan(recover_tail)?;
        Ok(store)
    }

    fn scan(&mut self, recover_tail: bool) -> Result<()> {
        let paths = files(&self.dir)?;
        for (i, path) in paths.iter().enumerate() {
            let file = open_regular(path, false, false)?;
            let mut reader = BufReader::new(file);
            let mut offset = 0u64;
            loop {
                let mut line = Vec::new();
                (&mut reader)
                    .take((MAX_RECORD + 1) as u64)
                    .read_until(b'\n', &mut line)?;
                if line.is_empty() {
                    break;
                }
                ensure!(
                    line.len() <= MAX_RECORD,
                    "oversized record in {}",
                    path.display()
                );
                if !line.ends_with(b"\n") {
                    ensure!(
                        recover_tail && i + 1 == paths.len(),
                        "incomplete final record in {}; use serve --recover-tail after review",
                        path.display()
                    );
                    self.quarantine(path, offset, &line)?;
                    break;
                }
                let (event, hash) = decode(&line)
                    .with_context(|| format!("{} at byte {offset}", path.display()))?;
                ensure!(
                    event.sequence == self.sequence + 1,
                    "sequence gap or reordering in {}",
                    path.display()
                );
                ensure!(
                    event.previous == self.head,
                    "broken hash chain in {}",
                    path.display()
                );
                let timestamp = OffsetDateTime::parse(&event.recorded_at, &Rfc3339)?;
                ensure!(
                    day(timestamp) == path.file_name().unwrap().to_string_lossy(),
                    "record date does not match filename"
                );
                ensure!(
                    self.last_time.is_none_or(|last| timestamp >= last),
                    "recording time moved backwards"
                );
                ensure!(
                    !self.seen.contains_key(&event.id),
                    "duplicate event id: {}",
                    event.id
                );
                let request = Append {
                    id: event.id.clone(),
                    kind: event.kind,
                    data: event.data,
                };
                let receipt = Receipt {
                    id: event.id.clone(),
                    sequence: event.sequence,
                    hash: hash.clone(),
                    file: day(timestamp),
                };
                self.seen.insert(
                    event.id,
                    Seen {
                        fingerprint: request.fingerprint()?,
                        receipt,
                    },
                );
                self.sequence = event.sequence;
                self.head = Some(hash);
                self.last_time = Some(timestamp);
                offset += line.len() as u64;
            }
            // A complete record may have survived a crash before its original
            // sync/ack. Make recovered records durable before accepting retries.
            reader.get_ref().sync_all()?;
        }
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    fn quarantine(&self, path: &Path, offset: u64, suffix: &[u8]) -> Result<()> {
        let backup = self
            .dir
            .join(format!(".strata-recovery-{}.bin", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup)?;
        file.write_all(suffix)?;
        file.sync_all()?;
        File::open(&self.dir)?.sync_all()?;
        let original = open_regular(path, true, false)?;
        original.set_len(offset)?;
        original.sync_all()?;
        eprintln!(
            "strata: quarantined {} incomplete bytes from {} to {}",
            suffix.len(),
            path.display(),
            backup.display()
        );
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
        let filename = day(now);
        let receipt = Receipt {
            id: request.id.clone(),
            sequence: event.sequence,
            hash: hash.clone(),
            file: filename.clone(),
        };
        // A failed write or sync has an uncertain outcome. Never continue from
        // the old in-memory head; reopening must revalidate the actual disk.
        self.poisoned = true;
        self.write_record(&filename, &bytes)?;
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

    fn write_record(&self, filename: &str, bytes: &[u8]) -> Result<()> {
        let path = self.dir.join(filename);
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
            "daily log must be a regular, non-hard-linked file"
        );
        file.write_all(bytes)?;
        file.sync_all()?;
        File::open(&self.dir)?.sync_all()?;
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

/// Offline verification. An optional independently trusted earlier directory
/// detects tail deletion or a consistently rehashed replacement history.
pub fn verify(dir: impl AsRef<Path>, baseline: Option<&Path>) -> Result<Verification> {
    ensure!(dir.as_ref().is_dir(), "store does not exist");
    let store = Store::open(dir)?;
    let mut result = store.status();
    if let Some(baseline) = baseline {
        ensure!(baseline.is_dir(), "baseline does not exist");
        let previous = Store::open(baseline)?;
        for (id, old) in &previous.seen {
            let Some(current) = store.seen.get(id) else {
                bail!("baseline event missing: {id}");
            };
            ensure!(
                current.receipt == old.receipt,
                "baseline event changed: {id}"
            );
        }
        result.baseline_events = Some(previous.sequence);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

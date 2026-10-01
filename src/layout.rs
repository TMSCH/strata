//! Shared validation for disk files and immutable Git snapshots.
use crate::record::{Append, Event, MAX_RECORD, Receipt, day, decode};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io::{BufRead, Read},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(crate) struct Record {
    pub event: Event,
    pub hash: String,
    pub bytes: Vec<u8>,
    pub file: String,
}

#[derive(Default)]
pub(crate) struct Snapshot {
    pub records: BTreeMap<u64, Record>,
}

pub(crate) fn date(name: &str) -> Result<()> {
    ensure!(name.is_ascii() && name.len() == 10, "invalid date: {name}");
    let parsed = time::Date::parse(
        name,
        time::macros::format_description!("[year]-[month]-[day]"),
    )?;
    ensure!(parsed.to_string() == name, "invalid date: {name}");
    Ok(())
}

fn hash_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Returns the date and, for a loose event, its expected hash.
pub(crate) fn location(name: &str) -> Result<(&str, Option<&str>)> {
    if let Some((date_part, file)) = name.split_once('/') {
        date(date_part)?;
        let hash = file
            .strip_suffix(".json")
            .context("loose event must end in .json")?;
        ensure!(hash_name(hash), "invalid event filename: {name}");
        Ok((date_part, Some(hash)))
    } else {
        let stem = name
            .strip_suffix(".jsonl")
            .context("unexpected store file")?;
        let date_part = if let Some((d, suffix)) = stem.split_once("--") {
            ensure!(hash_name(suffix), "invalid archive suffix");
            d
        } else {
            stem
        };
        date(date_part)?;
        Ok((date_part, None))
    }
}

pub(crate) fn auxiliary(name: &str) -> bool {
    matches!(name, ".gitignore" | ".gitattributes" | "README.md")
}

impl Snapshot {
    pub fn add(&mut self, name: &str, mut reader: impl BufRead) -> Result<()> {
        let (date, expected_hash) = location(name)?;
        let mut archive_hash = Sha256::new();
        let mut count = 0;
        let mut last_sequence = None;
        loop {
            let mut bytes = Vec::new();
            (&mut reader)
                .take((MAX_RECORD + 1) as u64)
                .read_until(b'\n', &mut bytes)?;
            if bytes.is_empty() {
                break;
            }
            ensure!(bytes.len() <= MAX_RECORD, "oversized record in {name}");
            archive_hash.update(&bytes);
            let (event, hash) =
                decode(&bytes).with_context(|| format!("invalid record in {name}"))?;
            let timestamp = OffsetDateTime::parse(&event.recorded_at, &Rfc3339)?;
            ensure!(
                &day(timestamp)[..10] == date,
                "record date does not match {name}"
            );
            if let Some(expected) = expected_hash {
                ensure!(hash == expected, "hash does not match filename: {name}");
            }
            ensure!(
                last_sequence.is_none_or(|last| event.sequence > last),
                "archive records out of order: {name}"
            );
            last_sequence = Some(event.sequence);
            count += 1;
            ensure!(
                expected_hash.is_none() || count == 1,
                "loose file contains multiple events"
            );
            if let Some(old) = self.records.get_mut(&event.sequence) {
                ensure!(
                    old.bytes == bytes,
                    "conflicting records at sequence {}",
                    event.sequence
                );
                // Prefer the archive as the receipt location during overlap.
                if expected_hash.is_none() {
                    old.file = name.to_owned();
                }
            } else {
                self.records.insert(
                    event.sequence,
                    Record {
                        event,
                        hash,
                        bytes,
                        file: name.to_owned(),
                    },
                );
            }
        }
        ensure!(
            expected_hash.is_none() || count == 1,
            "empty loose event file: {name}"
        );
        if let Some((_, suffix)) = name
            .strip_suffix(".jsonl")
            .and_then(|stem| stem.split_once("--"))
        {
            ensure!(
                format!("{:x}", archive_hash.finalize()) == suffix,
                "archive hash does not match filename: {name}"
            );
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let mut previous = None;
        let mut last_time = None;
        let mut ids = HashSet::new();
        for (index, record) in self.records.values().enumerate() {
            ensure!(record.event.sequence == index as u64 + 1, "sequence gap");
            ensure!(
                record.event.previous.as_deref() == previous,
                "broken hash chain"
            );
            let timestamp = OffsetDateTime::parse(&record.event.recorded_at, &Rfc3339)?;
            ensure!(
                last_time.is_none_or(|last| timestamp >= last),
                "recording time moved backwards"
            );
            ensure!(
                ids.insert(&record.event.id),
                "duplicate event id: {}",
                record.event.id
            );
            previous = Some(record.hash.as_str());
            last_time = Some(timestamp);
        }
        Ok(())
    }

    pub fn preserves(&self, baseline: &Self) -> Result<()> {
        for (seq, old) in &baseline.records {
            let current = self
                .records
                .get(seq)
                .with_context(|| format!("baseline event missing: {}", old.event.id))?;
            ensure!(
                old.bytes == current.bytes,
                "baseline event changed: {}",
                old.event.id
            );
        }
        Ok(())
    }

    pub fn status(&self) -> crate::store::Verification {
        crate::store::Verification {
            events: self.records.len() as u64,
            head: self.records.last_key_value().map(|(_, r)| r.hash.clone()),
            baseline_events: None,
        }
    }
}

impl Record {
    pub fn receipt(&self) -> Receipt {
        Receipt {
            id: self.event.id.clone(),
            sequence: self.event.sequence,
            hash: self.hash.clone(),
            file: self.file.clone(),
        }
    }
    pub fn request(&self) -> Append {
        Append {
            id: self.event.id.clone(),
            kind: self.event.kind.clone(),
            data: self.event.data.clone(),
        }
    }
}

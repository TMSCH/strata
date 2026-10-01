use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Limits apply to serialized bytes, not characters.
pub const MAX_REQUEST: usize = 64 * 1024;
pub const MAX_RECORD: usize = MAX_REQUEST + 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Append {
    /// Reuse this identifier when retrying a request whose outcome is unknown.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

impl Append {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty() && self.id.len() <= 128,
            "id must be 1..128 bytes"
        );
        ensure!(
            self.id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)),
            "id must contain only ASCII letters, digits, '-', '_', '.', ':'"
        );
        ensure!(
            !self.kind.is_empty() && self.kind.len() <= 128,
            "type must be 1..128 bytes"
        );
        ensure!(
            self.kind
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)),
            "type must contain only ASCII letters, digits, '-', '_', '.', ':'"
        );
        ensure!(self.data.is_object(), "data must be a JSON object");
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_REQUEST,
            "request exceeds 64 KiB"
        );
        Ok(())
    }

    pub(crate) fn fingerprint(&self) -> Result<String> {
        Ok(digest(b"strata-request-v1\0", &serde_json::to_vec(self)?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub id: String,
    pub sequence: u64,
    pub hash: String,
    pub file: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub version: u8,
    pub sequence: u64,
    pub id: String,
    pub recorded_at: String,
    pub previous: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    hash: String,
    event: Box<RawValue>,
}

pub(crate) fn digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(domain);
    h.update(bytes);
    format!("{:x}", h.finalize())
}

pub(crate) fn encode(event: &Event) -> Result<(Vec<u8>, String)> {
    let raw = serde_json::to_string(event)?;
    let hash = digest(b"strata-event-v1\0", raw.as_bytes());
    let envelope = Envelope {
        hash: hash.clone(),
        event: RawValue::from_string(raw)?,
    };
    let mut bytes = serde_json::to_vec(&envelope)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= MAX_RECORD, "event is too large");
    Ok((bytes, hash))
}

pub(crate) fn decode(line: &[u8]) -> Result<(Event, String)> {
    ensure!(line.ends_with(b"\n"), "incomplete final record");
    let envelope: Envelope = serde_json::from_slice(line)?;
    ensure!(
        envelope.hash == digest(b"strata-event-v1\0", envelope.event.get().as_bytes()),
        "event hash mismatch"
    );
    let event: Event = serde_json::from_str(envelope.event.get())?;
    ensure!(
        event.version == 1,
        "unsupported event version {}",
        event.version
    );
    // The v1 writer has exactly one encoding. This also rejects duplicate payload
    // keys, alternate envelope whitespace and unknown fields in stored records.
    let (expected, _) = encode(&event)?;
    ensure!(line == expected, "noncanonical v1 record encoding");
    Append {
        id: event.id.clone(),
        kind: event.kind.clone(),
        data: event.data.clone(),
    }
    .validate()?;
    if let Some(previous) = &event.previous {
        ensure!(
            previous.len() == 64
                && previous
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid previous hash"
        );
    }
    let timestamp = OffsetDateTime::parse(&event.recorded_at, &Rfc3339)?;
    if timestamp.offset() != time::UtcOffset::UTC {
        bail!("recorded_at must be UTC");
    }
    Ok((event, envelope.hash))
}

pub(crate) fn day(timestamp: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}.jsonl",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day()
    )
}

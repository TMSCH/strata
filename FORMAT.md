# Strata file format v1

## Files and order

Authoritative data is the set of `YYYY-MM-DD.jsonl` files directly in the store
root. All other names are local metadata or unrelated files. Any filename ending
in `.jsonl` must be a valid ASCII date filename. Dates use UTC and years 0000–9999.
Read daily files in filename order, and records within a file in line order.
Empty daily files are valid, including one created before a failed first append.

Each completed record is a compact UTF-8 JSON object followed by exactly one LF
byte. No BOM, blank lines, CRLF, or trailing whitespace. Each line is at most
69,632 bytes including LF. Parsers must bound record size before allocating.

```json
{"hash":"<64 lowercase SHA-256 hex digits>","event":{"version":1,"sequence":1,"id":"example-food-1","recorded_at":"2026-09-30T12:00:00Z","previous":null,"type":"client.food","data":{"client":"alice","content":"steak and fries"}}}
```

This example is illustrative; `tests/fixtures/2026-09-30.jsonl` contains an actual
independently generated hash vector.

## Hash definition

`hash = hex_lower(SHA256(b"strata-event-v1\0" || event_bytes))`

`event_bytes` is the exact UTF-8 substring occupied by the `event` JSON object,
including its braces. The domain contains one trailing NUL byte. The hash excludes
the envelope and final LF. `previous` is inside the hashed event, so hashes link
all records across days. First record: sequence 1 and previous null. Later records:
sequence exactly one greater and previous equal to the preceding record's hash.

The envelope keys are written in order `hash`, `event`; the event fields are in
exactly the order shown. The v1 writer uses compact `serde_json` serialization;
payload object keys are sorted by Rust string ordering, arrays retain their order,
and Unicode is written directly except characters escaped by JSON serialization.
Numbers follow `serde_json::Value` semantics (i64/u64/f64). Use strings for exact
decimals and integers beyond that range. Input whitespace/order is normalized;
duplicate keys in input payload objects follow serde_json's last-value behavior.
The append request has exactly `id`, `type`, `data`; extra request fields fail.

Verification hashes the stored raw event bytes first, then requires the record to
match v1 serialization byte-for-byte. Unknown or duplicate envelope/event fields,
alternate encodings, and noncanonical stored payloads are rejected. This encoding
is **not RFC 8785/JCS**. Compatible future writers must retain v1 encoding; dependency
updates must pass golden vectors. Introducing another encoding needs a new version.

## Semantics

- `id`: caller request identity, globally unique in this store. 1–128 restricted
  ASCII bytes, as documented in README. Retrying identical normalized type/data
  with that ID returns the original receipt. A different payload/type is an error.
- `recorded_at`: daemon UTC time in RFC 3339; optional fractional seconds. Must not
  precede the previous record and must match the filename date. It does not mean
  that the described activity occurred at that time.
- `type`: application event type; Strata does not impose an application schema.
- `data`: JSON object, including any client identity or occurrence timestamps.
  Those are assertions supplied by the caller, not authenticated identities.
- `sequence`: the store's global order. Timestamps and IDs do not replace it.

Receipts have `id`, `sequence`, `hash`, and `file`. There is no mutable head file
or authoritative index. All state can be reconstructed from the JSONL files.

## Socket protocol

One compact JSON append request plus LF per Unix stream connection. Maximum
65,536 request bytes before LF. Send `{"id":"...","type":"...","data":{...}}`.
A response is `{"receipt":{...},"error":null}` or
`{"receipt":null,"error":"description"}`, followed by LF; the server closes the
connection. Extra pipelined requests are unsupported. Invalid requests do not
append. A transport error has an unknown outcome: retry the same request ID.

## Trust and recovery

Checks verify a chain's internal consistency, not its completeness relative to a
past state. Trusted baseline verification checks all earlier receipts, including
their hashes/sequences/file names. A removed tail or fully rehashed replacement
can otherwise be self-consistent. Git anchoring belongs to the trusted orchestrator.

There is no merge or compaction format. The only supported mutation of existing
bytes is explicit operator recovery of an unterminated final suffix, after durable
quarantine. That is not a claim that the suffix was never acknowledged: a later
filesystem corruption or hostile truncation can resemble an interrupted write.

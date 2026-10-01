# Strata

A small, local, append-only event store for personal agents. Rust library and
CLI. One daemon, many clients, daily JSONL files you can read with ordinary tools.

```text
events/
  2026-09-29.jsonl
  2026-09-30.jsonl
  2026-10-01.jsonl
```

Events are hash-linked across days. The writer never edits completed records.
There is no query language, read server, index database, compaction, or distributed
coordination. Intended for modest personal-agent histories on macOS and Linux.
This is an initial implementation, not an independently audited storage system.

## Install

With Rust installed:

```sh
cargo install --locked --git https://github.com/TMSCH/strata strata-log
```

For reproducible installs, add `--rev <reviewed-commit>` or `--tag v0.1.0` once
that tag has been released. Release archives are built for Linux x86-64 (static musl) and
macOS ARM64; see [releases](https://github.com/TMSCH/strata/releases). No crates.io
publication is required. The library crate is named `strata`; its package is
`strata-log`.

## Start the daemon

Use a short socket path in a private directory (Unix socket path limits apply).
The socket must live outside the data directory when agents have read-only access
to that directory.

```sh
mkdir -p "$HOME/.strata-run"
chmod 700 "$HOME/.strata-run"
export STRATA_SOCKET="$HOME/.strata-run/strata.sock"
strata serve --dir ./events --socket "$STRATA_SOCKET"
```

The store directory is created if missing; its parent must already exist.
The daemon runs in the foreground. Your supervisor can restart it; stale socket
files are handled on startup. It locks the store and socket separately. A second
writer is rejected. Stop it with your supervisor or Ctrl-C; every acknowledged
append has already been synchronized, so shutdown needs no buffer flush.

## Append

From another shell with the same `STRATA_SOCKET`:

```sh
printf '%s\n' '{"client":"alice","content":"steak and fries"}' |
  strata append --type client.food --id meal-2026-10-01-lunch
```

Input is one JSON object on stdin. Successful stdout is a JSON receipt containing
`id`, `sequence`, `hash`, and the daily `file`. Errors and diagnostics use stderr;
errors exit nonzero. There are no terminal prompts.

`--id` is optional: a UUID is generated and printed to stderr before contacting
the daemon. Prefer a stable caller-provided ID. If the result is uncertain (timeout,
disconnection, or crash), retry **the same ID and content**. This returns the
original receipt, even after restart. Reusing an ID for different content fails.
IDs are unique across the whole store. A retry identifier does not guarantee
exactly-once execution of anything outside Strata.

Types and IDs are 1–128 ASCII bytes from letters, digits, `-`, `_`, `.`, and `:`.
The serialized request is limited to 64 KiB, including its ID/type envelope.
Data must be an object. Do not put secrets into a history you plan to retain in Git.

## Read ordinary files

Each line is a complete event envelope, including its type and payload:

```sh
rg 'client.food' events/
jq -c 'select(.event.type == "client.food") | .event.data' events/*.jsonl
```

Text search is approximate; JSON parsing gives exact field matches. Dates are
UTC recording dates, not dates described by the event. Store an occurrence date
inside `data` when needed. A live reader may encounter an incomplete last line
while an append is in progress: ignore that suffix and retry. For a stable snapshot,
stop the writer or read an accepted Git commit. There is intentionally no
`strata read` command.

## Verify and use Git

Stop the daemon before verification, commits, checkout, reset, or restore. Never
change its files underneath a running writer. Add these patterns to your data
repository's `.gitignore` (adjust the `events/` prefix):

```gitignore
events/.strata.lock
events/.strata-recovery-*.bin
```

Keep recovery backups locally until reviewed. Do not delete the lock file while
any writer or verifier is running. Commit the JSONL files as ordinary UTF-8 text;
do not apply formatters, line-ending conversion, or Git filters. A suitable
`.gitattributes` rule is:

```gitattributes
events/*.jsonl -text -merge
```

This keeps bytes intact and surfaces divergent edits as conflicts. It does not
enforce append-only history. Independent branch writes/merges are unsupported.
Delegate agents may use separate code worktrees while sharing one Strata daemon.

```sh
strata verify --dir ./events
strata verify --dir ./events --baseline /path/to/trusted-earlier-events
```

`verify` checks the full chain and rejects malformed or modified records. An
independently trusted baseline additionally requires every earlier event to remain
present and unchanged. Export the last accepted Git commit into a separate
writable temporary directory and pass its events directory as the baseline.
The trusted commit/push owner should enforce this before accepting a new snapshot.
A baseline is only useful if agents cannot replace it or select an older one.
Verification acquires an exclusive lock and requires write access for the local
lock file; it does not modify event contents.

Git backups contain only committed events. Acknowledged but uncommitted events
are locally durable, not remotely backed up. Plain hashes cannot detect removal
of an unreferenced tail or replacement of the entire chain without a trusted
baseline. They do not authenticate authors or prove event truth.

## Interrupted writes

Startup verifies all completed records and fails closed on corruption. An
incomplete final line also causes startup to fail by default. After reviewing it:

```sh
strata serve --dir ./events --socket "$STRATA_SOCKET" --recover-tail
```

This explicit option saves the unterminated suffix in a unique
`.strata-recovery-*.bin` file, synchronizes that backup, then truncates only the
suffix in the latest daily file. It never repairs a malformed newline-terminated
record or an incomplete older day. A valid record missing only its final newline
is still an incomplete suffix and will be quarantined. Retry the original request
ID after recovery. If damage affected an acknowledged record, restore from trusted
history rather than treating recovery as proof that no data was lost.

## Guarantees and boundaries

- One writer serializes requests from four fixed connection workers. Idle request
  reads time out after five seconds. No unbounded per-client thread creation.
- Before success, the event bytes and containing directory are synchronized with
  `File::sync_all`. Errors stop further writes until restart. Actual power-loss
  durability depends on the OS, local filesystem, and hardware honoring sync.
- A complete record surviving an uncertain write is verified and synchronized on
  reopen before a retry can succeed. Interrupted suffixes are never silently lost.
- Global sequences and previous hashes cross daily file boundaries. A backwards
  system clock rejects new events until it catches up; retries still work.
- Files and socket default to mode `0600`; new store directories to `0700`.
  Symlink/nonregular/hard-linked log and lock files are rejected. Parent directories
  and their ancestors must be controlled by the operator.
- A daemon under the same unrestricted account is **not a security boundary**.
  Sandbox agents to read the logs and connect to the socket, while denying file
  writes, directory replacement, daemon control, and trusted Git credentials.
  Separate OS identities/ACLs are another deployment option; permission setup is
  the operator's responsibility. Socket peers are not individually authenticated.
- Local filesystems only. NFS, cloud-sync folders, hostile filesystem writers,
  simultaneous Git mutations, encryption, and independent replica merges are out
  of scope. Resource exhaustion and privileged tampering are not prevented.
- Startup scans history. Memory retains IDs, request fingerprints, and receipts,
  not all payloads. This intentionally favors simplicity over large-store scaling.

## Library

Trusted applications can embed the same writer without a daemon:

```rust,no_run
use strata::{Append, Store};

let mut store = Store::open("events")?;
let receipt = store.append(Append {
    id: "meal-1".into(),
    kind: "client.food".into(),
    data: serde_json::json!({"content": "steak and fries"}),
})?;
# Ok::<(), anyhow::Error>(())
```

Only the trusted process should embed `Store`; agent clients use `append` through
the Unix socket. See [FORMAT.md](FORMAT.md) for the portable representation.

## Development and releases

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

The toolchain and dependency lockfile are committed. Tests cover concurrent CLI
clients, duplicate retries, restart, day rollover, tampering, baseline preservation,
unsafe paths, invalid inputs, and every byte cut of a representative interrupted
record. These simulate torn writes; they do not emulate physical power failure.

CI tests macOS ARM64 and Linux x86-64, checks the declared minimum Rust version,
and audits dependencies against RustSec. Release workflow dispatch builds downloadable
archives without publishing. Pushing a reviewed `v<package-version>` tag builds and
tests both targets, then publishes archives and SHA-256 checksums to GitHub Releases.
No automatic installation or integration with Nester is performed.

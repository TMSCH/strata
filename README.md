# Strata

A small, local, append-only event store for personal agents. Rust library and
CLI. One daemon, many clients, ordinary JSON files with daily JSONL archives.

```text
events/
  2026-09-29.jsonl
  2026-09-30.jsonl
  2026-10-01/
    <event-hash>.json
    <another-event-hash>.json
```

Events are hash-linked across days. The writer never edits completed records.
New events are published as complete, immutable files. After a UTC day ends, the
daemon archives its events into one immutable JSONL file, then removes the loose
files. There is no query language, read server, index database, or distributed
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
`id`, `sequence`, `hash`, and a current `file` location. Errors and diagnostics use stderr;
errors exit nonzero. There are no terminal prompts.

`--id` is optional: a UUID is generated and printed to stderr before contacting
the daemon. Prefer a stable caller-provided ID. If the result is uncertain (timeout,
disconnection, or crash), retry **the same ID and content**. This returns the
same event identity, sequence, and hash, even after restart. The `file` hint can
change after compaction; it is not a permanent path. Reusing an ID for different content fails.
IDs are unique across the whole store. A retry identifier does not guarantee
exactly-once execution of anything outside Strata.

Types and IDs are 1–128 ASCII bytes from letters, digits, `-`, `_`, `.`, and `:`.
The serialized request is limited to 64 KiB, including its ID/type envelope.
Data must be an object. Do not put secrets into a history you plan to retain in Git.

## Read ordinary files

Each line is a complete event envelope, including its type and payload:

```sh
rg -g '*.json' -g '*.jsonl' 'client.food' events/
```

Text search is approximate; JSON parsing gives exact field matches. For example,
on an accepted Git snapshot, collect both loose files and archives, deduplicate
by hash, and order by sequence:

```sh
find events -path 'events/.strata-tmp' -prune -o -type f \
  \( -name '*.json' -o -name '*.jsonl' \) -exec cat {} + |
  jq -sc 'unique_by(.hash) | sort_by(.event.sequence)[] |
    select(.event.type == "client.food") | .event.data'
```

Dates are UTC recording dates, not dates described by the event. Store an occurrence
date inside `data` when needed. Published files never contain partial writes.
During compaction, live readers can see duplicate representations or a file can
vanish between listing and opening it. Retry for casual reads; use an accepted Git
snapshot when completeness matters. There is intentionally no `strata read` command.

## Verify and use Git

**The daemon can keep running during staging and commits.** The trusted commit
owner verifies the staged snapshot before accepting it. Stop the daemon before
checkout, reset, restore, or other operations that replace its working files.

Add these patterns to your data repository's `.gitignore` (adjust `events/`):

```gitignore
events/.strata.lock
events/.strata-tmp/
events/.strata-recovery-*.bin
```

Do not delete the lock file while a writer or verifier is running. Do not apply
formatters, line-ending conversion, or Git filters to records. In `.gitattributes`:

```gitattributes
events/**/*.json -text -merge
events/*.jsonl -text -merge
```

The trusted Git owner runs:

```sh
git add -A -- events
strata verify --staged --repo . --path events
git commit -m "Record agent events"
```

`--path` is relative to the repository root. Verification captures the index once
with `git write-tree` and reads the blobs from that immutable tree. It checks the
full chain and that every event in **HEAD** remains present with identical bytes.
It needs no daemon shutdown or store lock. The JSON result includes `events`,
`head`, `baseline_events`, `tree`, and `baseline_commit`.

The commit owner must exclusively control the index and baseline reference during
this sequence. No other staging or hook may change the index between verification
and commit. For automated orchestration, create a commit directly from the returned
`tree` using `git commit-tree`, then advance the branch with a compare-and-swap
against the expected old commit. Strata verifies snapshots; it does not own Git
commits, pushes, credentials, or that coordination.

If concurrent append/compaction produces an incomplete staged snapshot, verification
fails: stage again and retry. A successful check can represent an earlier complete
prefix; new events published too late for that scan go into the next commit.
Identical events in both loose files and an archive count once. A successful plain
`git add` or `git commit` alone does not establish these guarantees.

For the first commit of an unborn repository, explicitly use `--initial`. If HEAD
already exists but contains no events, normal verification works. To anchor against
a different independently trusted commit, use `--baseline-ref <commit>`. The
trusted owner chooses the baseline; agents must not be able to replace it or select
an older one. Independent divergent branch writes/merges remain unsupported.
Delegate agents can share the single daemon while working on separate code branches.

Offline verification is also available and takes the writer lock:

```sh
strata verify --dir ./events
strata verify --dir ./events --baseline /path/to/trusted-earlier-events
```

Git backups contain only committed events. Acknowledged but uncommitted events
are locally durable, not remotely backed up. Plain hashes cannot detect removal
of an unreferenced tail or replacement of the entire chain without a trusted
baseline. They do not authenticate authors or prove event truth.

## Compaction and interrupted writes

The daemon checks for completed days at startup, on a new-day append, and every
minute. Library users can call `Store::compact()` periodically. Compaction publishes
and synchronizes the archive **before** deleting any loose files. Restart validates
and deduplicates any overlap, then finishes cleanup. Archived records are never
rewritten. Old daily JSONL stores remain readable; if a day already has an archive
and additional loose events, compaction writes an immutable
`YYYY-MM-DD--<archive-sha256>.jsonl` supplement.

Publication uses a synchronized private temporary file and an atomic no-replace
rename. Unsupported filesystems fail rather than using a non-atomic fallback.
Interrupted, unpublished files in `.strata-tmp/` are ignored; the operator may remove
that directory while the daemon is stopped. Startup fails on malformed **published**
files and never truncates them. The old `--recover-tail` option is removed. Preserve
any damaged legacy file for review and restore from trusted history before restart.

## Guarantees and boundaries

- One writer serializes requests from four fixed connection workers. Idle request
  reads time out after five seconds. No unbounded per-client thread creation.
- Before success, the event bytes and containing directory are synchronized with
  `File::sync_all`. Errors stop further writes until restart. Actual power-loss
  durability depends on the OS, local filesystem, and hardware honoring sync.
- A complete published record surviving an uncertain write is verified and
  synchronized on reopen before a retry can succeed. Private unpublished bytes
  are not authoritative.
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
  external mutations of live event files, encryption, and independent replica merges are out
  of scope. Resource exhaustion and privileged tampering are not prevented.
- Steady-state memory retains IDs, request fingerprints, and receipts. Startup,
  compaction, and verification temporarily load event bytes to validate and
  deduplicate the history. This favors simplicity for small personal histories.

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
unsafe paths, invalid inputs, every byte cut of a representative unpublished
record, interrupted compaction, working-tree/index disagreement, and staged history
preservation. These simulate interrupted operations, not physical power failure.

CI tests macOS ARM64 and Linux x86-64, checks the declared minimum Rust version,
and audits dependencies against RustSec. Release workflow dispatch builds downloadable
archives without publishing. Pushing a reviewed `v<package-version>` tag builds and
tests both targets, then publishes archives and SHA-256 checksums to GitHub Releases.
No automatic installation or integration with Nester is performed.

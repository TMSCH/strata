# Changelog

## 0.1.1

- Added opt-in `serve --group-readable` and `--group-append` for separate writer
  and agent accounts. Group members can read published events and submit appends;
  event files remain non-writable by the group. Defaults remain private.
- Existing records require an offline permission migration when enabling group
  reading; the daemon does not change permissions across historical files.
- Staged verification now reads Git objects through one batch process per tree,
  improving performance for histories with many loose event files.

## 0.1.0

Updated SHA-256 support to sha2 0.11 while preserving v1 event hashes, request
fingerprints, and archive filenames. Updated pinned GitHub Actions; manual release
builds now also download and verify both packaged artifacts before publication.

Compaction now detects Git repositories and retains loose events until the exact
daily archives are committed in HEAD. Ordinary Git commits and bundle backups need
no Strata-specific verification hook. Git failures defer cleanup without stopping
appends; standalone stores still clean up after durable publication.

Rust library, Unix-socket daemon, append CLI, durable acknowledgements, and
restart-safe retry IDs. Events are published as immutable hash-named JSON files
and compacted into daily JSONL archives. Verification supports offline directories
and immutable Git index snapshots, with trusted baseline preservation.

Existing v1 daily JSONL records remain readable without re-encoding. Compaction
changes receipt file locations while preserving event identity. Published files
are never truncated; the earlier `--recover-tail` option has been removed.

Tag-triggered releases build and test Linux x86-64 and macOS ARM64 binaries.

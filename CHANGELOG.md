# Changelog

## 0.1.0 (unreleased)

Rust library, Unix-socket daemon, append CLI, durable acknowledgements, and
restart-safe retry IDs. Events are published as immutable hash-named JSON files
and compacted into daily JSONL archives. Verification supports offline directories
and immutable Git index snapshots, with trusted baseline preservation.

Existing v1 daily JSONL records remain readable without re-encoding. Compaction
changes receipt file locations while preserving event identity. Published files
are never truncated; the earlier `--recover-tail` option has been removed.

Tag-triggered releases build and test Linux x86-64 and macOS ARM64 binaries.

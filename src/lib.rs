#![doc = include_str!("../README.md")]
//! A single-writer, hash-linked JSONL store. See README.md and FORMAT.md.
#![cfg(unix)]
pub mod protocol;
pub mod record;
pub mod store;
pub use record::{Append, Receipt};
pub use store::{Store, verify};

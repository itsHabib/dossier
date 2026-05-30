//! Reference implementation of the Agent Project Protocol over an on-disk
//! markdown corpus. The wire spec lives in PROTOCOL.md; the on-disk
//! format lives in LAYOUT.md.

pub mod domain;
pub mod server;
pub mod store;

pub use store::{ArtifactListFilter, FsStore, Store, StoreError, Version, Versioned};

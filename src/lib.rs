//! Reference implementation of the Agent Project Protocol over an on-disk
//! markdown corpus. The wire spec lives in PROTOCOL.md; the on-disk
//! format lives in LAYOUT.md.

pub mod domain;
pub mod s3store;
pub mod server;
pub mod store;

pub use s3store::{S3Config, S3Store};
pub use store::{ArtifactListFilter, FsStore, Store, StoreError, Version, Versioned};

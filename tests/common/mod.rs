//! Shared helpers for integration tests. Lives under `tests/common/` so
//! each integration-test crate can include it as `mod common;`.

// `unreachable_pub` and `clippy::redundant_pub_crate` disagree about
// what visibility to give helpers in `tests/common/mod.rs`. The module
// is included via `mod common;` (not `pub mod`), so `pub` is technically
// unreachable from outside the test crate but `pub(crate)` is flagged
// as redundant. Standard idiom: keep `pub`, silence the unreachable
// warning.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test helpers; see comment above"
)]

use std::fs;

use dossier::store::FsStore;
use tempfile::TempDir;

/// A fresh, isolated corpus rooted in a `TempDir`. The `TempDir` is held
/// by the caller so the directory lives as long as the test does;
/// dropping it removes the corpus.
pub fn fresh_corpus() -> (TempDir, FsStore) {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
    let store = FsStore::open(tmp.path()).expect("open fresh corpus");
    (tmp, store)
}

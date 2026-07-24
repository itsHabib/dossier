//! Property tests for artifact `meta` JSON round-trip (`S3Store` JSONL codec).

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use proptest::prelude::*;

use crate::domain::{
    Artifact, ARTIFACT_META_MAX_KEYS, ARTIFACT_META_MAX_KEY_LEN, ARTIFACT_META_MAX_SERIALIZED_LEN,
    ARTIFACT_META_MAX_VALUE_LEN,
};

use super::parse_artifacts_jsonl;

fn meta_key() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[a-z][a-z0-9_]{0,15}").expect("meta key regex")
}

fn meta_value() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[a-z0-9][a-z0-9 _-]{0,63}").expect("meta value regex")
}

fn capped_meta_map() -> impl Strategy<Value = BTreeMap<String, String>> {
    proptest::collection::hash_map(meta_key(), meta_value(), 0..=ARTIFACT_META_MAX_KEYS)
        .prop_filter("meta within caps", |meta| {
            if meta.len() > ARTIFACT_META_MAX_KEYS {
                return false;
            }
            for (key, value) in meta {
                if key.len() > ARTIFACT_META_MAX_KEY_LEN {
                    return false;
                }
                if value.len() > ARTIFACT_META_MAX_VALUE_LEN {
                    return false;
                }
            }
            let Ok(serialized) = serde_json::to_string(meta) else {
                return false;
            };
            serialized.len() <= ARTIFACT_META_MAX_SERIALIZED_LEN
        })
        .prop_map(|meta| meta.into_iter().collect())
}

fn utc_time() -> impl Strategy<Value = DateTime<Utc>> {
    (0i64..4_000_000_000)
        .prop_map(|secs| DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now))
}

fn artifact_with_meta() -> impl Strategy<Value = Artifact> {
    (
        proptest::string::string_regex("art_[A-Z0-9]{26}").expect("art id"),
        proptest::string::string_regex("prj_[A-Z0-9]{26}").expect("prj id"),
        proptest::string::string_regex("[a-z0-9_-]{1,16}").expect("kind"),
        proptest::string::string_regex(r"[a-z0-9:/._-]{1,32}").expect("ref"),
        proptest::string::string_regex(r"[A-Za-z0-9][A-Za-z0-9 _-]{0,16}").expect("label"),
        utc_time(),
        capped_meta_map(),
    )
        .prop_map(
            |(id, project, kind, reference, label, linked_at, meta)| Artifact {
                id,
                project,
                task: String::new(),
                kind,
                reference,
                label,
                linked_at,
                actor: "human:test".to_owned(),
                meta,
            },
        )
}

proptest! {
    #[test]
    fn s3_artifact_jsonl_meta_roundtrip(artifact in artifact_with_meta()) {
        let line = serde_json::to_string(&artifact).expect("serialize artifact");
        let parsed = parse_artifacts_jsonl(&line).expect("parse jsonl line");
        prop_assert_eq!(parsed.len(), 1);
        let row0 = parsed.first().expect("row 0");
        prop_assert_eq!(&row0.meta, &artifact.meta);
        if artifact.meta.is_empty() {
            prop_assert!(!line.contains("\"meta\""));
        }
    }
}

//! Property tests for artifact `meta` round-trip through `FsStore`.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use std::collections::BTreeMap;

use chrono::Utc;
use proptest::prelude::*;

use crate::domain::{
    new_id, Artifact, Project, ProjectStatus, ARTIFACT_META_MAX_KEYS, ARTIFACT_META_MAX_KEY_LEN,
    ARTIFACT_META_MAX_SERIALIZED_LEN, ARTIFACT_META_MAX_VALUE_LEN,
};
use crate::store::{FsStore, Store};

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

proptest! {
    #[test]
    fn fs_store_artifact_meta_roundtrip(meta in capped_meta_map()) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let store = FsStore::open(tmp.path()).expect("open");
        let now = Utc::now();
        let project = Project {
            id: new_id("prj"),
            slug: "prop-meta".to_owned(),
            title: "Prop".to_owned(),
            description: String::new(),
            status: ProjectStatus::Planning,
            created_at: now,
            updated_at: now,
            created_by: "human:test".to_owned(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            store.put_project(&project, None).await.expect("project");
            let artifact = Artifact {
                id: new_id("art"),
                project: project.id.clone(),
                task: String::new(),
                kind: "verdict".to_owned(),
                reference: "gate://prop".to_owned(),
                label: "lbl".to_owned(),
                linked_at: now,
                actor: "human:test".to_owned(),
                meta: meta.clone(),
            };
            store.put_artifact(&artifact).await.expect("put");
            let listed = store.list_artifacts(&project.slug).expect("list");
            let got = listed
                .into_iter()
                .find(|a| a.id == artifact.id)
                .expect("found");
            assert_eq!(got.meta, meta);
        });
    }
}

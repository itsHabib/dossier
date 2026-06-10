//! End-to-end tests for the real `dossier serve` MCP-over-stdio transport.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "integration tests"
)]

mod common;

use std::path::Path;

use common::fresh_corpus;
use rmcp::{
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt, TokioChildProcess},
    ServiceExt,
};
use serde_json::{json, Value};

fn dossier_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dossier")
}

async fn connect_mcp(corpus: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let transport = TokioChildProcess::new(tokio::process::Command::new(dossier_bin()).configure(
        |cmd| {
            cmd.arg("serve")
                .arg("--corpus")
                .arg(corpus)
                .env_remove("DOSSIER_CORPUS")
                .env_remove("DOSSIER_BACKEND")
                .kill_on_drop(true);
        },
    ))
    .expect("spawn dossier serve");

    // Bound the handshake: a server that starts but never answers `initialize`
    // should fail the test, not hang the whole `cargo test` job indefinitely.
    tokio::time::timeout(std::time::Duration::from_secs(30), ().serve(transport))
        .await
        .expect("initialize handshake timed out after 30s")
        .expect("mcp initialize handshake")
}

fn tool_result_value(result: rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .or_else(|| {
            result
                .content
                .first()
                .and_then(|content| content.as_text())
                .and_then(|text| serde_json::from_str(&text.text).ok())
        })
        .expect("tool result payload")
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_initialize_handshake_succeeds() {
    let (tmp, _store) = fresh_corpus();
    let client = connect_mcp(tmp.path()).await;
    client.cancel().await.expect("shutdown dossier serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_tools_list_registers_core_verbs() {
    let (tmp, _store) = fresh_corpus();
    let client = connect_mcp(tmp.path()).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();

    for expected in ["project.create", "task.create", "artifact.link"] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}; registered tools: {names:?}"
        );
    }

    client.cancel().await.expect("shutdown dossier serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_project_create_then_get_round_trips() {
    let (tmp, _store) = fresh_corpus();
    let client = connect_mcp(tmp.path()).await;

    let create_args = json!({
        "slug": "stdio-e2e",
        "title": "MCP stdio E2E",
        "description": "booted over stdio",
        "actor": "test:mcp-stdio"
    })
    .as_object()
    .expect("create args object")
    .clone();

    let created = client
        .call_tool(CallToolRequestParams::new("project.create").with_arguments(create_args))
        .await
        .expect("project.create");
    let created = tool_result_value(created);
    assert_eq!(created["slug"], Value::String("stdio-e2e".to_owned()));
    assert_eq!(created["title"], Value::String("MCP stdio E2E".to_owned()));

    let get_args = json!({ "slug": "stdio-e2e" })
        .as_object()
        .expect("get args object")
        .clone();

    let fetched = client
        .call_tool(CallToolRequestParams::new("project.get").with_arguments(get_args))
        .await
        .expect("project.get");
    let fetched = tool_result_value(fetched);
    assert_eq!(
        fetched["project"]["slug"],
        Value::String("stdio-e2e".to_owned())
    );
    assert_eq!(
        fetched["project"]["title"],
        Value::String("MCP stdio E2E".to_owned())
    );
    assert_eq!(fetched["project"]["id"], created["id"]);

    client.cancel().await.expect("shutdown dossier serve");
}

**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-08
**Related**: dossier task `mcp-stdio-transport-e2e` (id: `tsk_01KTJSJSXCMTF3T5N7BNBQDCFP`), [docs/follow-ups.md](../../follow-ups.md), `tests/cli_subcommands.rs`, `src/bin/dossier.rs`

# tests: MCP-over-stdio transport E2E (boot the real server) — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Tests | new `tests/mcp_stdio_e2e.rs` | ~200 | 100 |
| Config (0×) | `Cargo.toml` `[dev-dependencies]` if a client/transport crate is needed | ~3 | 0 |
| **Total** | | | **~100** |

Band: amazing (could drift toward ideal if a hand-rolled JSON-RPC harness is needed — see note). Test-only; no production change.

## Goal

dossier's primary interface is the rmcp MCP server over stdio (`dossier serve`), but nothing tests it end-to-end. `tests/cli_subcommands.rs::cli_serve_requires_explicit_corpus` only spawns `serve` to assert it refuses without `--corpus` (arg-validation); every other test exercises `MeshService` / `Store` directly. Transport- and registration-level regressions slip through — e.g. the documented `get_info()` reporting `name: "rmcp"` gotcha, tool-schema drift, or the `Json<T>` return-wrapper bound (all in CLAUDE.md "Common gotchas") would pass unit tests but break a real MCP client. Add an E2E test that boots the real binary and drives it over stdio.

## Behavior / fix

Add `tests/mcp_stdio_e2e.rs` that:

1. Spawns `dossier serve --corpus <tempdir-with-.dossier>` as a child process (build a tempdir with the `.dossier/` marker so `FsStore::open` succeeds — see the "Corpus marker required" gotcha).
2. Performs the MCP `initialize` handshake over the child's stdin/stdout.
3. Calls `tools/list` and asserts the verbs are registered — at least `project_create`, `task_create`, `artifact_link`.
4. Does a `tools/call` round-trip: `project_create` then `project_get`, asserting the JSON-RPC result reflects the created project (slug/title round-trips back).

Use the rmcp client against a child-process transport if that's ergonomic; otherwise a minimal hand-rolled JSON-RPC-over-stdio harness (write framed requests, read framed responses) is acceptable. Keep it on the `fs` backend — no MinIO.

If startup latency makes the test slow enough to drag `make check`, gate it behind a marker (e.g. `#[ignore]` with a documented `cargo test --test mcp_stdio_e2e -- --ignored`, or a feature flag) and wire it into CI explicitly. Default to running it in `make check` if it's fast.

## Acceptance

- `cargo test --test mcp_stdio_e2e` boots the real binary, completes `initialize` + a `project_create` → `project_get` round-trip over stdio, and asserts the created project is readable back.
- Runs in the default `make check` (or, if gated for speed, runs in CI via an explicit invocation).
- Green locally and in CI.

## Test plan

- `mcp_stdio_initialize_handshake_succeeds`
- `mcp_stdio_tools_list_registers_core_verbs`
- `mcp_stdio_project_create_then_get_round_trips`

(Consolidate into fewer test fns if a single boot serves multiple assertions — booting the child once and asserting in sequence is fine.)

## Non-goals

- The S3 backend over the transport — `fs` proves the transport wiring; S3 is covered by the CAS gate.
- HTTP/SSE transport — not shipped.
- Exhaustive per-verb E2E coverage — one `create` → `get` round-trip plus `tools/list` proves registration + dispatch; the rest stay at the unit/integration layer.

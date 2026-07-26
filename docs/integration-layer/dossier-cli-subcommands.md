**Status**: draft
**Owner**: human:michael
**Date**: 2026-05-19
**Related**: dossier task `dossier-cli-subcommands` (id: `tsk_01KS6QYMT05KTE1W8F08EA4YZC`), phase `mcp-workstation/integration-layer`
**Repo**: `pers/dossier`
**Branch**: `integration-layer/dossier-cli`

# dossier-cli-subcommands — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (Rust, clap routing + thin wrappers) | `src/bin/dossier.rs`, possibly `src/store.rs` for idempotency tweaks | ~200-250 | ~200-250 |
| Tests | new integration tests under `tests/` for CLI subcommands + idempotency | ~120-180 | ~60-90 |
| **Total** | | | **~260-340** |

Band: **amazing** (per dossier's PR sizing convention, <500 weighted).

## Goal

Add three one-shot CLI subcommands to the `dossier` binary so the integration-layer hooks can write to dossier from bash without spawning an MCP client per call. Today dossier exposes only `serve`; the verbs only exist through MCP. This task fills that gap by routing CLI subcommands to the same `FsStore` methods the MCP server already calls.

Side benefit: dossier becomes usable from CI scripts, ad-hoc shell, and other tools — closing an unwritten gap surfaced repeatedly in the workbench-friction log.

## Behavior

Add four new subcommands to `dossier.exe` (a fifth read verb, `artifact_list`, followed later in PR #101 — item 5):

1. **`dossier task_complete --id <task-id> [--note "<text>"] [--actor <actor>]`**
   - Calls the same logic backing the MCP `task.complete` verb.
   - Default `--actor` = `cli:$USER` (fallback `cli` if USER unset).
   - **Idempotency:** task already `done` → exit 0, stderr message `task <id> already complete (no-op)`, no corpus mutation.
   - Returns updated frontmatter as JSON on stdout.

2. **`dossier task_update --id <task-id> --note "<text>" [--actor <actor>]`**
   - Appends a structured note to the task's `## Notes` section. Matches MCP `task.update` semantics.
   - Default `--actor` = `cli:$USER`.
   - Notes are append-only — CLI does NOT dedupe. Caller (hooks) responsible for not double-firing on the same event.
   - Returns updated frontmatter as JSON.

3. **`dossier artifact_link --project <slug> --kind <kind> --ref <ref> [--task <task-id>] [--label <text>] [--actor <actor>]`**
   - Calls the same logic backing the MCP `artifact.link` verb.
   - **Idempotency:** same `(project, task, kind, ref)` already exists → exit 0, stderr message, no duplicate.
   - If today's underlying `FsStore` method doesn't dedupe, ALSO add that property — the MCP verb gets the same fix.
   - Default `--actor` = `cli:$USER`.
   - Returns the artifact entry as JSON.

4. **`dossier task_list [--project <slug>] [--phase <slug>] [--status <status>...] [--assignee <actor>] [--limit <N>]`**
   - Calls the same logic backing the MCP `task.list` verb. Mirrors its filter surface (subset is fine for v1).
   - `--status` is a comma-separated list (`todo,in_progress`) or repeatable flag.
   - `--phase` requires `--project` (same constraint as the MCP verb).
   - No `--project` → list across all projects.
   - Returns the matching tasks as a JSON array on stdout.
   - **Idempotency:** read-only, trivially idempotent.

5. **`dossier artifact_list --project <slug> [--task <task-id>] [--kind <kind>] [--ref <ref>]`** *(added later — PR #101, the state-substrate read path)*
   - Calls the same logic backing the MCP `artifact.list` verb. `--project` is required.
   - `--ref` is an exact-match filter and AND-composes with `--task` / `--kind`.
   - Empty flag values are treated as "no filter" (matching the MCP handler), so a shell-expanded `--kind ""` does not filter for an impossible empty kind.
   - Returns the matching artifacts as a JSON array on stdout.
   - **Idempotency:** read-only, trivially idempotent.

## Implementation sketch

Extend the clap subcommand enum in `src/bin/dossier.rs`:

```rust
enum Command {
    Serve { corpus: PathBuf },
    TaskComplete { id: String, note: Option<String>, actor: Option<String> },
    TaskUpdate { id: String, note: String, actor: Option<String> },
    ArtifactLink { project: String, kind: String, ref_: String, task: Option<String>, label: Option<String>, actor: Option<String> },
    TaskList { project: Option<String>, phase: Option<String>, status: Vec<String>, assignee: Option<String>, limit: Option<usize> },
}
```

Each non-`serve` subcommand:
1. Resolve corpus root: `--corpus` flag, `DOSSIER_CORPUS` env var, or walk-up looking for `.dossier/` marker (match `serve` behavior).
2. Open the store.
3. Call appropriate `FsStore` method.
4. Print result JSON to stdout using the same serialization as the MCP server.
5. Exit non-zero on actionable errors.

## Acceptance

All four subcommands work end-to-end against `pers/dossier-state`, and the MCP server still behaves identically.

## Test plan

- Unit: `task_complete` on todo task transitions to done; on done task returns no-op (idempotency).
- Unit: `artifact_link` same-tuple is no-op (idempotency); different tuple appends to `artifacts.jsonl`.
- Unit: `task_update` appends note; duplicate calls append duplicates (per design).
- Unit: `task_list` returns matching tasks as JSON array; filter args (`--project`, `--phase`, `--status`) behave as MCP equivalent does.
- Integration: CLI output JSON matches MCP server's tool response JSON for the same verb.
- Existing tests pass unchanged; new CLI tests cover the new surface.

## Non-goals

- New verbs (only CLI for existing ones).
- On-disk format changes.
- Auto-discovering corpus location beyond `serve`'s current behavior.
- Daemon mode / long-running CLI; each invocation is one-shot.

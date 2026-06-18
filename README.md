# dossier

[![CI](https://github.com/itsHabib/dossier/actions/workflows/ci.yml/badge.svg)](https://github.com/itsHabib/dossier/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Project memory for the solo developer. One markdown-on-disk corpus tracking
projects, phases (design docs / TDDs), tasks, and the artifacts (PRs, commits,
files) that shipped them — across an entire portfolio, queryable and mutable
through any LLM. You point an agent at it and it answers "what's open in
`<project>`?", "show me the auth design doc", "what did I close last week?" —
and creates the structure as work starts, without you typing in a UI.

dossier exposes the corpus two ways: an **MCP server** (`dossier serve`) that
gives any LLM the full read + write verb set over stdio, and a thin **CLI**
for the handful of verbs worth scripting from a shell or a CI hook
(`task_complete`, `task_update`, `artifact_link`, `task_list`). The corpus
itself is the source of truth — plain markdown you can grep and edit by hand;
the server re-reads it on every call.

See [docs/vision.md](docs/vision.md) for the longer "what + why + what we're
explicitly not building."

## Quick start

```sh
# 1. Build + install the `dossier` binary
cargo install --path .

# 2. Pick (or create) a corpus directory
mkdir -p ~/dossier-corpus/.dossier   # the .dossier/ marker is all you need

# 3. Register dossier with Claude Code as an MCP server
claude mcp add dossier -- "$(which dossier)" serve --corpus ~/dossier-corpus

# 4. Open a new Claude Code session
#    The verbs are now available: project.create, phase.add, task.create,
#    task.claim, task.update, task.complete, artifact.link, plus the
#    matching list / get reads.
```

### Claude Desktop

Same binary, different config — add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "dossier": {
      "command": "/absolute/path/to/dossier",
      "args": ["serve", "--corpus", "/absolute/path/to/dossier-corpus"]
    }
  }
}
```

Restart Claude Desktop; the dossier tools show up in the tools menu.

Ask Claude to create a project for one of your repos and link a PR:

> Create a project in dossier for "tower" with slug `tower` and title "Tower —
> worktree observer". Add a phase `01-spec` titled "v0 spec". Then link
> PR #42 from `itsHabib/tower` as an artifact under that phase.

That's it. No `init` command needed — the write verbs scaffold the
directory tree as you go.

## Example session

Once dossier is wired into a Claude Code session, an agent typically drives
project memory without operator prompting — creating structure as work starts
and linking artifacts when PRs land:

```
# Create a project
mcp__dossier__project_create {"slug": "wellness-ai", "title": "Wellness AI", "actor": "human:michael"}

# Add a phase + task
mcp__dossier__phase_add {"project": "wellness-ai", "slug": "hyrox-coach-mvp", "title": "HYROX coach MVP", "actor": "human:michael", "owner": "human:michael"}
mcp__dossier__task_create {"project": "wellness-ai", "phase": "hyrox-coach-mvp", "slug": "auth-flow", "title": "auth flow design", "actor": "human:michael"}

# Link a PR when it merges
mcp__dossier__artifact_link {"project": "wellness-ai", "task": "tsk_...", "kind": "pr", "ref": "https://github.com/.../pull/42", "label": "PR #42 — auth flow", "actor": "human:michael"}
```

The `mcp__dossier__<verb>` names are the Claude-Code-prefixed surface form;
[PROTOCOL.md](PROTOCOL.md) describes the underlying verbs as `project.create` /
`phase.add` / `task.create` / `artifact.link`. Tool args are JSON objects.

## Layout

A corpus is any directory with a `.dossier/` marker. Inside:

```
~/dossier-corpus/
  .dossier/
    config.toml          # reserved; may be empty
  projects/
    <project-slug>/
      project.md         # YAML frontmatter + markdown body
      phases/
        01-<phase-slug>.md
      tasks/
        <task-id>-<task-slug>.md
      artifacts.jsonl    # append-only
```

### Corpus location

Each operator keeps their corpus wherever they like — dossier only cares
about the path you pass to `--corpus`. A common convention is
`~/pers/dossier-state/` (e.g. `mkdir -p ~/pers/dossier-state/.dossier` then
`dossier serve --corpus ~/pers/dossier-state`); the write verbs create
everything else. The in-repo `.dossier/` marker and `projects/` directory
form a checked-in fixture corpus used by tests — they're a valid corpus, just
not the one an operator edits day-to-day.

On disk: [project](LAYOUT.md#projectmd) ·
[phase](LAYOUT.md#phasesnn-slugmd) ·
[task](LAYOUT.md#tasksid-slugmd) ·
[artifact](LAYOUT.md#artifactsjsonl).
Full format in [LAYOUT.md](LAYOUT.md). The corpus is the source of truth
— humans grep and edit the markdown directly, and the mesh re-reads it
on every call.

## Storage backend

dossier serves from a swappable storage backend, chosen by `DOSSIER_BACKEND`
(default `fs`):

- **`fs`** (default) — the local markdown-on-disk corpus described above. Pass
  `--corpus <path>`; nothing else needed.
- **`s3`** — an S3-compatible object store (AWS S3, Cloudflare R2, MinIO). The
  same corpus layout is mirrored under a key prefix, and concurrent writes are
  safe via per-object compare-and-swap (`If-Match` / `If-None-Match` ETags).

```sh
DOSSIER_BACKEND=s3 \
DOSSIER_S3_BUCKET=my-bucket \
AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… \
dossier serve --corpus /tmp/ignored      # see the --corpus note below
```

S3 config is read from the environment:

| Variable | Required | Notes |
| --- | --- | --- |
| `DOSSIER_S3_BUCKET` | yes | |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | yes | static keys |
| `DOSSIER_S3_ENDPOINT` | no | for MinIO / R2 / other S3-compatible endpoints |
| `DOSSIER_S3_REGION` | no | falls back to `AWS_REGION` / `AWS_DEFAULT_REGION`, then `us-east-1` |
| `DOSSIER_S3_PREFIX` | no | key prefix within the bucket; defaults to the root |
| `DOSSIER_S3_FORCE_PATH_STYLE` | no | defaults to `true` when an endpoint is set (MinIO) |

> **Note:** `serve` currently still requires a `--corpus <path>` argument even
> for `s3` — the path is ignored when the backend is `s3` (a follow-up will let
> the S3 server boot from env alone).

The S3 backend is wired end-to-end; its no-lost-update guarantee under
concurrent writers is validated by a CAS stress gate (GO against MinIO — see
[the gate spec](docs/features/cloud-backend/phase-1/cas-lost-update-validation-gate.md)).
The hosted multi-writer tier built on it is in progress — see
[the cloud-backend design](docs/features/cloud-backend/spec.md).

## Verbs

The MCP server registers 16 tools. Reads never mutate; writes route through the
runtime-guarded task state machine and atomic file writes.

| Read | what it does |
| --- | --- |
| `project.list` | list projects, filtered by status / body / date ranges |
| `project.get` | one project with its phases, tasks, artifacts, and body |
| `phase.list` | list phases (cross-project or scoped), bodies included |
| `task.list` | list tasks, filtered by status / assignee / phase / dates |
| `task.get` | fetch a single task by id, no project slug needed |
| `artifact.list` | list a project's linked artifacts (PRs, commits, files) |
| `search` | one ranked case-insensitive substring pass over project / phase / task titles + bodies |

| Write | what it does |
| --- | --- |
| `project.create` | new project (unique lowercase slug; server stamps id + timestamps) |
| `project.update` | edit title / description / status (slug is immutable) |
| `phase.add` | append or insert a phase (`after_phase` for ordering) |
| `phase.update` | edit phase title / body / status / owner |
| `task.create` | new task, optionally anchored to a phase |
| `task.claim` | sole entry to `claimed`; same-actor re-claim is a no-op |
| `task.update` | edit body / status / append a progress note |
| `task.complete` | sole entry to `done`; implicit-claims from `todo` / `claimed` |
| `artifact.link` | append-only link of a PR / commit / file / url / run / doc |

Data model: [project](PROTOCOL.md#project) ·
[phase](PROTOCOL.md#phase) ·
[task](PROTOCOL.md#task) ·
[artifact](PROTOCOL.md#artifact) ·
[task state machine](PROTOCOL.md#task-state-machine).
See [PROTOCOL.md](PROTOCOL.md) for the full spec.

### CLI

`dossier serve` is the main surface. For shell / CI use, three writes plus
`task_list` are exposed as one-shot subcommands that print their result as JSON
— they share the `--corpus` flag (or `DOSSIER_CORPUS`, or a walk-up search for
`.dossier/`):

```sh
dossier task_list --project tower --status in_progress
dossier task_update --id tsk_… --note "blocked on review"
dossier task_complete --id tsk_… --note "merged in #42"
dossier artifact_link --project tower --kind pr --ref https://github.com/itsHabib/tower/pull/42 --label "PR #42"
```

## Develop

```sh
make check        # fmt-check + clippy --all-targets --all-features -- -D warnings + test
make fmt          # apply rustfmt
make test         # cargo test
make build        # debug build
make release      # release build
```

CI runs `make check` on every PR. Lint discipline + conventions live in
[CLAUDE.md](CLAUDE.md).

## Architecture

A strict one-directional layering — each layer depends only on the one below,
so the storage backend is swappable and the MCP transport is just the top edge:

```
bin  (src/bin/dossier.rs)   CLI + stdio MCP transport, arg parsing
 │
server  (src/server.rs)     MeshService — the 16 verbs over rmcp
 │
store   (src/store.rs)      Store trait + FsStore  ┐ swappable backend
        (src/s3store.rs)    S3Store (CAS via ETags)┘ behind one trait
 │
domain  (src/domain.rs)     pure types + the task state machine, no I/O
```

| Module | Responsibility |
| --- | --- |
| `domain` | primitives + status enums + state machine. No I/O; 1:1 with PROTOCOL.md. |
| `store` | `Store` trait and `FsStore` — read/write the on-disk corpus per LAYOUT.md. |
| `s3store` | `S3Store` — the same layout on an S3-compatible object store, with compare-and-swap writes. |
| `server` | `MeshService` — wraps a `Store` and registers every MCP verb. |
| `bin` | CLI entry, stdio transport, the one-shot subcommands. |

## Why this exists

A solo dev with a dozen side projects has the same recurring problem:
*where did I write that down?* The design doc for the auth migration
lives in one repo, the TDD for the data pipeline in another, and the
PRs are scattered across GitHub. dossier consolidates the project-state
plane in plain markdown that humans grep and LLMs query.

The discipline is to be excellent at one thing — *track project docs and tasks
for a solo developer, and let an LLM answer questions about them* — before
adding a second. So a lot stays deliberately out of scope:

- **Semantic / vector / RAG search lives in the LLM consumer, not the store.**
  dossier ships literal substring `search` and nothing fancier; embedding
  indexes belong to whatever agent queries the corpus.
- **Natural-language understanding lives in the LLM, not dossier.** The server
  provides structured truth; the model is the conversational layer.
- **History / audit lives in git.** No `last_updated_by`, no audit log — the
  corpus is checked in, so `git log` already has it.
- **No web UI, no conflict-detection engine, no cross-project dependency
  graph, no multi-implementer protocol machinery.** Markdown + grep + an
  MCP-aware agent is the interface; single-actor is the assumption for v0.

The full framing — and the running list of things we're *not* building — is in
[docs/vision.md](docs/vision.md).

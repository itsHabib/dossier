# Agent Project Protocol (APP) — v0

A protocol over MCP for agents and humans to coordinate on project work.
Defines the primitives ("what is a project, phase, task") and verbs ("claim
this task, post progress, link this PR") that any conforming server exposes
and any conforming agent can call.

The protocol is the contract. Servers (storage backends, indices, UIs) and
agents (implementers, readers, orchestrators) are interchangeable as long as
they speak it.

## Status

v0 — minimal core. Designed to be the smallest surface that lets a single
implementer agent (e.g. ship) drive a project end-to-end while a human or
reader agent observes. Search, semantic queries, multi-tenant auth,
cross-project relationships, and decision tracking are explicitly deferred.

## Roles

The protocol does not enforce roles — any agent may call any verb — but
three patterns recur:

- **Orchestrator** — creates projects, adds phases, defines tasks. Often a
  human via a frontend, sometimes an agent kicking off work from a higher-
  level goal.
- **Implementer** — claims tasks, posts progress, links artifacts, marks
  tasks complete. Examples: ship, a code-writing agent, a human in a
  terminal.
- **Reader** — queries state to answer questions ("what's going on with
  project X", "what's blocking task Y"). Examples: a manager-facing chat
  agent, a CLI status command.

## Primitives

All primitives carry: `id` (server-assigned, stable), `created_at`,
`updated_at` (ISO-8601), and `actor` on every write (free-form string —
agent name or human handle; servers may validate, the protocol does not).

### Project

A unit of work with a goal. Holds phases, tasks, and artifacts.

| field         | type                                              | notes                       |
|---------------|---------------------------------------------------|-----------------------------|
| `id`          | string                                            | server-assigned             |
| `slug`        | string                                            | client-supplied, unique     |
| `title`       | string                                            |                             |
| `description` | string (markdown)                                 | the design doc body         |
| `status`      | `planning` \| `active` \| `paused` \| `done` \| `abandoned` |                |
| `created_at`  | timestamp                                         |                             |
| `updated_at`  | timestamp                                         |                             |

### Phase

An ordered subdivision of a project. Phases are linear, not a graph.

| field        | type                                        | notes                  |
|--------------|---------------------------------------------|------------------------|
| `id`         | string                                      | server-assigned        |
| `project_id` | string                                      |                        |
| `title`      | string                                      |                        |
| `body`       | string (markdown)                           | phase doc / acceptance |
| `order`      | integer                                     | dense, server-managed  |
| `status`     | `pending` \| `active` \| `done` \| `skipped` |                        |

### Task

A discrete piece of work an implementer can claim and complete.

| field         | type                                                              | notes                         |
|---------------|-------------------------------------------------------------------|-------------------------------|
| `id`          | string                                                            |                               |
| `project_id`  | string                                                            |                               |
| `phase_id`    | string \| null                                                    | nullable for project-wide tasks |
| `title`       | string                                                            |                               |
| `body`        | string (markdown)                                                 | spec / acceptance criteria    |
| `status`      | `todo` \| `claimed` \| `in_progress` \| `blocked` \| `done` \| `cancelled` |                       |
| `assignee`    | string \| null                                                    | actor that holds the claim    |
| `claimed_at`  | timestamp \| null                                                 |                               |
| `completed_at`| timestamp \| null                                                 |                               |
| `notes`       | array of `{ actor, body, posted_at }`                             | append-only progress log      |

### Artifact

A pointer to something concrete the work produced or depends on.

| field         | type                                                          | notes                               |
|---------------|---------------------------------------------------------------|-------------------------------------|
| `id`          | string                                                        |                                     |
| `project_id`  | string                                                        |                                     |
| `task_id`     | string \| null                                                |                                     |
| `kind`        | `commit` \| `pr` \| `file` \| `url` \| `run` \| `doc`         | extensible                          |
| `ref`         | string                                                        | sha / url / path / run id           |
| `label`       | string                                                        | short human-readable                |
| `linked_at`   | timestamp                                                     |                                     |

## Verbs (MCP tools)

Names are dot-segmented. All return the affected resource on success. All
writes are idempotent on `(actor, request_id)` if `request_id` is supplied.

### Project

- `project.create` — `{ slug, title, description }` → Project
- `project.get` — `{ id | slug }` → Project (with phases, tasks, artifacts inlined)
- `project.list` — `{ status?, limit?, cursor? }` → list of Project (without children)
- `project.update` — `{ id, title?, description?, status? }` → Project

### Phase

- `phase.add` — `{ project_id, title, body, after_phase_id? }` → Phase
- `phase.update` — `{ id, title?, body?, status? }` → Phase
- `phase.list` — `{ project_id }` → ordered list of Phase

### Task

- `task.create` — `{ project_id, phase_id?, title, body }` → Task (status=`todo`)
- `task.claim` — `{ id, actor }` → Task (status=`claimed`, assignee=actor). Fails if already claimed by another actor.
- `task.update` — `{ id, body?, status?, note? }` → Task. `note` appends to log.
- `task.complete` — `{ id, note? }` → Task (status=`done`, completed_at=now)
- `task.list` — `{ project_id?, phase_id?, status?, assignee? }` → list of Task

### Artifact

- `artifact.link` — `{ project_id, task_id?, kind, ref, label }` → Artifact
- `artifact.list` — `{ project_id?, task_id?, kind? }` → list of Artifact

## Task state machine

```
todo ──claim──▶ claimed ──update(in_progress)──▶ in_progress
  │                │                                  │
  │                ▼                                  ▼
  │             cancelled                          blocked ──update(in_progress)──▶ in_progress
  │                                                   │
  └──────────────────────────────────────────────────▶ done (via complete)
```

- `cancelled` is terminal.
- `done` is terminal.
- Any non-terminal state may transition to `cancelled` via `task.update`.
- `blocked` may carry a note explaining the block.

## Identity & provenance

Every write includes `actor` (string). Every resource exposes the `actor`
that created it and the actor of each subsequent mutation. The protocol
does not authenticate actors — that is the server's concern. Implementers
SHOULD use stable identifiers (`ship`, `claude-code:michael`, `human:mh`)
so readers can attribute work.

## Versioning

Servers expose `protocol_version` (semver) in their MCP `initialize`
response. Clients SHOULD refuse to operate against a server with an
incompatible major version. Tool names and field names are stable within
a major version; new optional fields and new tools may appear in minor
versions.

This document is `v0` — pre-1.0, breaking changes are allowed between
revisions. A breaking change requires bumping the v0.x revision and
updating this document.

## Out of scope (v0)

Deliberately deferred to keep the core small. Each is a candidate for a
future minor version once a real consumer demands it:

- **Search / semantic query** across projects (lives in a storage layer
  consumer, not the protocol)
- **Decisions** as a first-class primitive (currently just a task or an
  artifact-of-kind=doc)
- **Cross-project links / dependencies** (out: keep projects independent for v0)
- **Permissions / multi-tenant auth** (server concern)
- **Notifications / subscriptions / streaming** (poll for now)
- **Rich attachments** (use artifacts pointing at external storage)
- **Time tracking, estimates, sprints** (workflow conventions on top, not
  protocol primitives)

## Conformance

A server is conforming if it implements every verb above, validates inputs
against the schemas, and respects the state machine. A client is conforming
if it identifies itself with a stable `actor` and uses verbs as defined.
Strict beats permissive — reject unknown fields rather than silently
ignoring them, so protocol drift surfaces fast.

# Vision

dossier is **project memory for the solo developer**. One place to keep
the design docs, TDDs, and task notes for every project across the
portfolio — readable as plain markdown, queryable through any LLM, and
linkable to the PRs and commits that actually shipped the work.

## Why

A solo dev with a dozen side projects has the same recurring problem:
*where did I write that down?* The design doc for the auth migration
lives in one repo, the TDD for the data pipeline in another, the rough
"things to think about" list for the third in a notes app, and the
actual PRs are scattered across GitHub.

dossier consolidates the project-state plane:

- **One on-disk format** (markdown + YAML frontmatter) that humans grep
  and edit directly, no DB to keep alive.
- **Four primitives** that map to how you actually think about work:
  project, phase, task, artifact.
- **MCP server** wrapping the corpus, so any LLM agent can query and
  mutate it — Claude, Cursor, agent SDK, whatever.

The corpus is the source of truth. The mesh is just a typed API over a
folder of markdown.

## Samurai sword, not swiss army

The discipline is to be excellent at one thing before adding the
second. The one thing is: **track project docs and tasks for a solo
developer, and let an LLM answer questions about them.** Everything
that doesn't directly serve that gets cut or deferred.

### Explicitly NOT building (yet, possibly ever)

- **Conflict detection** (same-assignee multi-claim, stale-claim
  warnings, slug-similarity heuristics). Enterprise problems.
- **Multi-implementer concerns** (`request_id` idempotency, conformance
  language, protocol-version negotiation). Single-actor today.
- **Audit logs / `last_updated_by`**. Git history covers it.
- **Search / RAG inside dossier**. LLMs already do retrieval over MCP
  tool outputs — build the query engine in the LLM, not the store.
- **Web UI**. Markdown + grep + an MCP-aware agent is the UI.
- **Cross-project relationship graphs / dependency tracking**. YAGNI.

These are not bad ideas. They're the wrong target *now*.

## What you have today

Shipped on `main`:

- **Corpus layout** (`LAYOUT.md`) — `.dossier/` marker, `projects/<slug>/{project.md,
  phases/, tasks/, artifacts.jsonl}`.
- **Read side** — `project.list / project.get / phase.list / task.list
  / artifact.list`.
- **Write side** — `project.create/update`, `phase.add/update`,
  `task.create/claim/update/complete` with a runtime-enforced state
  machine, slug uniqueness, atomic file writes, and structural guards
  (newline injection, `## Notes` collision, corrupt-state detection).
- **MCP surface** (`src/server.rs`) exposing every verb via `rmcp`.
- **Tests** — 47 unit, plus a `read_dogfood_corpus` test that pins the
  on-disk format to a real corpus (`projects/dossier/`).
- **CI** — fmt + clippy (Cheney-strict) + test on every PR. `@claude`,
  `@codex`, and Copilot bots review every PR.

## The shortlist of what's next

In order:

1. **`artifact.link`** (PR D, ~80 LOC) — wire `append_jsonl` into a new
   verb so tasks can point at the PRs / commits / files that shipped
   them. Directly serves "link to a PR."
2. **`dossier sync` / `init` CLI** — walk into a random repo, run one
   command, dossier scaffolds `.dossier/` from what it sees (README →
   `project.md`, `docs/features/*/spec.md` → phases, recent merged PRs
   → artifacts). The adoption unlock.
3. **Install dossier as an MCP server in Claude Code / Claude Desktop**
   — zero new code. Once #1 and #2 land, any Claude session can answer
   "what's open in roxiq?" / "show me the auth design doc" / "what
   tasks did I close last week?" by calling the existing verbs. The
   LLM is the natural-language layer; dossier provides the structured
   truth.

After #3, stop and watch real usage. Anything that looks like noise in
the corpus, or a question Claude can't answer well, becomes the next
target — chosen from evidence, not speculation.

## Docs layout

- [`vision.md`](vision.md) — this file. The high-level "what and why."
- [`features/<feature>/spec.md`](features/) — design spec per feature.
  Problem statement, scope, decisions, acceptance criteria.
- [`features/<feature>/plan.md`](features/) — execution plan with phase
  checkboxes when the spec implies multiple PRs (optional; the write-
  side spec embeds its PR breakdown directly).
- [`follow-ups.md`](follow-ups.md) — cross-cutting backlog of nice-to-
  haves discovered during review or implementation. Cleared
  opportunistically.

Specs and plans live under `docs/features/<feature>/`, not scattered
across the repo. Reference docs (external SDKs, protocols we depend
on) go directly under `docs/`.

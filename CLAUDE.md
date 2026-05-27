# dossier

Notes for agents working on this repo. Read before touching code.

dossier is **project memory for the solo developer** — one place to
track design docs, TDDs, and task notes across a portfolio, queryable
through any LLM, linkable to the PRs that shipped the work. Vision and
non-goals live in [docs/vision.md](docs/vision.md); read that first if
you haven't.

The implementation is a Rust MCP server (`dossier`) over a
markdown-on-disk corpus. The corpus is the source of truth; the mesh
is a typed API over a folder of markdown. On-disk format in
[LAYOUT.md](LAYOUT.md); data model in [PROTOCOL.md](PROTOCOL.md).

## State

Write side is shipped. Verbs available end-to-end: `project.list /
get / create / update`, `phase.list / add / update`, `task.list /
create / claim / update / complete`, `artifact.list`. The state
machine is runtime-guarded, atomic writes route through helpers, slug
validation is enforced on every create path.

`artifact.link` is the next chunk (~80 LOC). After that: a
`dossier init` / `sync` CLI that scaffolds a fresh corpus from
an existing repo. Conflict detection and other "enterprise" verbs are
explicitly deferred — see [docs/vision.md](docs/vision.md).

<!-- BEGIN dev-workbench (managed by /dev-workbench skill — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

Several MCP servers + skills are available in any Claude session on this machine — the dev-workflow infrastructure built across the portfolio. **This is dossier — the project-memory plane itself** — so the dossier verbs are the most directly relevant when working in this repo, alongside ship (workflow execution), huddle (multi-seat coordination), and the `/worktree-*` skill family for git worktrees. When the signal matches, **just call the verb**. Don't ask permission.

Dogfood reality: we track dossier's own work in the real corpus at `~/pers/dossier-state/projects/dossier/` (separate from the in-repo test fixture under `projects/dossier/`). When you call `phase.add` / `task.create` for dossier work, it lands there.

### dossier — project memory

Long-term home for what's planned, in-flight, and shipped across the portfolio. Projects → phases (design docs) → tasks → artifacts (PRs / commits / files). Markdown-on-disk corpus; the on-disk format IS the source of truth.

**Use proactively for:**

- *"What's the state of `<project>`?"* → `mcp__dossier__project_get { slug }`, then `mcp__dossier__phase_list` + `mcp__dossier__task_list { project, status: ["in_progress"] }`.
- *"I'm starting `<new chunk of work>`."* → `mcp__dossier__phase_add { project, slug, title, body }`.
- *"I need to do X"* / discrete actionable surface → `mcp__dossier__task_create { project, phase?, slug, title, body }` (status defaults to `todo`).
- User picks up a task → `mcp__dossier__task_claim { id, actor: "human:michael" }`. Re-claim by same actor is a no-op.
- Progress on a task → `mcp__dossier__task_update { id, status?, note?, ... }`. Append notes liberally — the corpus IS the working log.
- Open / merged PR → `mcp__dossier__artifact_link { project, task?, kind: "pr"|"commit", ref, label }` without being asked.
- *"Done with task X."* → `mcp__dossier__task_complete { id, note? }`.

**Don't use for:**

- Code-level work (write the code first; *then* `artifact_link` the PR).
- Anything that only matters within this session's scratch context.

### ship — workflow execution

Hands a task doc to a coding agent (cursor), persists what happened, lets you inspect / cancel / replay the run. Owns nothing about the workspace (the `/worktree-*` skills handle that) or the planning (dossier's job).

**Use proactively for:**

- *"Ship `<task doc>` against `<worktree>`."* → `mcp__ship__ship { workdir, docPath, repo, branch, runtime? }`. V2-async — returns `{ workflowRunId, status: "running" }` immediately. For cloud runs add `runtime: "cloud"` + `cloud: { repos: [{ url }], autoCreatePR: true, workOnCurrentBranch: false }`.
- *"What ran on `<repo>` recently?"* / *"What's still in flight?"* → `mcp__ship__list_workflow_runs { repo?, status?, limit? }`.
- *"What did `<wf id>` do?"* → `mcp__ship__get_workflow_run { workflowRunId }` (also accessible via the `ship://runs/{id}` resource).
- In-flight run needs to stop → `mcp__ship__cancel_workflow_run { workflowRunId }` (idempotent on terminal rows).

**Don't use for:**

- Creating the worktree (use `/worktree-add`).
- Writing the task doc (a normal file edit inside the worktree).
- Recording the merged PR back to project state (dossier `artifact_link`).
- Opening the PR — `mcp__ship__open_pr` is deprecated. For local runs, `gh pr create` from the worktree. For cloud runs, `autoCreatePR: true` opens it automatically; read the URL from `mcp__ship__get_workflow_run`.

### huddle — multi-agent / multi-seat coordination

Spins up a Slack channel + per-seat keys so multiple agents (or agent + human) can share a working context without polluting any one session's chat. Each "seat" gets a key it uses to post / read; the orchestrator (huddle creator) has full access via `huddleId`.

**Use proactively for:**

- *"Set up a coordination channel for `<purpose>` with `<N>` agents."* → `mcp__huddle__huddle_create { purpose, orchestrator: { id, displayName }, seats: [{ id, displayName }, ...], ttlHours? }`. Returns per-seat keys + Slack channel id.
- *"What huddles are open?"* → `mcp__huddle__huddle_list { active: true }`.
- *"Post an update into huddle `<id>`."* → `mcp__huddle__huddle_post { huddleId, body, key?, replyTo? }`. Orchestrator omits `key`; seats include their key.
- *"Catch up on the channel."* → `mcp__huddle__huddle_read { huddleId?, key?, since?, limit? }`.
- Done → `mcp__huddle__huddle_close { huddleId }` (archives the Slack channel + marks done).

**Don't use for:**

- One-off agent runs that don't need cross-agent coordination — just ship the task and read the events log.
- Long-term project memory (dossier owns that).

### playwright — browser automation

Headless / headed browser control via Playwright. Use when an agent task genuinely needs to interact with a web UI (login flow, scraping rendered DOM, screenshotting a page state) rather than hitting an API.

**Use proactively for:**

- *"Open `<url>` and check `<element>`."* → `mcp__plugin_playwright_playwright__browser_navigate { url }` then `..._browser_snapshot` (returns the accessibility tree) or `..._browser_take_screenshot`.
- *"Fill `<form>` and submit."* → `..._browser_fill_form { fields: [...] }` then `..._browser_click { ref }`.
- *"Capture network requests during `<flow>`."* → `..._browser_navigate` + `..._browser_network_requests` after the action.
- *"Run JS against the page."* → `..._browser_evaluate { code }`.

**Don't use for:**

- API testing — use `curl` / `gh` / a real HTTP client.
- Anything where the page is server-rendered and could be fetched via `WebFetch` instead.
- Tasks where the operator's actual Chrome session is needed (use the claude-in-chrome MCP for that — separate tier).

### `/work-driver` — drive agent-led impl end-to-end

Coordinates one or N parallel streams through the full loop: pre-flight worktrees, fan out via `ship.ship`, poll terminal states, verify cursor's auto-commit (or commit manually if absent), open PRs, drive review cycles, merge in dep order, cleanup. Reads a manifest produced by `/work-driver-prep` (the common case) or a list of one-off spec docs (ad-hoc). Canonical reviewer set per PR: `@copilot`, `@codex review`, `@claude review`, `@cursor review`.

**Triggers:** "drive this impl work", "run this through ship", "fire N parallel streams", "ship and merge", explicit `/work-driver`.

**Pair with:** `/work-driver-prep` when you have a batch of dossier tasks and want one spec doc per task + conflict-aware batching before fanning out.

### `/work-driver-prep` — spec docs + batched plan from a backlog of tasks

Takes a list of dossier tasks (or a phase slug) and emits one spec doc per task plus a structured `driver.md` manifest grouping the specs into parallel-safe batches. Removes the manual gap between "I have N todo tasks" and "I can invoke `/work-driver`."

**Triggers:** "ship the open follow-ups", "fan these tasks out", "prep work-driver", "set up the hygiene PRs", explicit `/work-driver-prep`.

**Pair with:** `/work-driver` (consumes the emitted manifest).

### `/shipped` — retrospective recap after a chunk of work lands

Post-`/work-driver` (or post-chip-blitz, post-manual-phase) summary: PRs merged + weighted-LOC, dossier task closures, friction-log delta, what changed on `main`, what's still open, suggested next moves. Auto-detects work-driver manifests for ground truth; falls back to git/gh/dossier signals otherwise.

**Triggers:** "what just shipped", "what did we ship", "what merged today", "post-run summary", "what now", explicit `/shipped`.

**Pair with:** `/status` — `/status` is in-flight, `/shipped` is the post-merge complement.

### `/status` — tight 4-section in-flight status update

What happened / What's next / What I recommend / What I need from you, 1-3 sentences each. Skips empty sections rather than padding. Use mid-session when you need to compress where you are without context-bloating the channel.

**Triggers:** "give me an update", "status", "where are we", "sitrep", "recap", "summarize the situation", explicit `/status`.

**Pair with:** `/shipped` when the work is fully landed and the ask is "what shipped" rather than "where are we."

### `/worktree-*` — manage secondary git worktrees

Thin skill family over plain `git worktree`. Use these instead of reaching for an MCP — they cover the verbs that mattered (add, list, remove, transfer, where) without an external state store. Default convention: branch name is user-chosen (no forced prefix); path is `<repo>/.claude/worktrees/<branch>/`.

- **`/worktree-add`** — *"spin up a worktree for <ticket>"* → creates `.claude/worktrees/<branch>/`, copies untracked CLAUDE.md if present
- **`/worktree-list`** — *"what worktrees do I have"* → branch, dirty state, optional PR/CI from `gh`
- **`/worktree-remove`** — *"clean up the worktree"* → dirty-state aware (commit-WIP / stash / discard)
- **`/worktree-transfer`** — *"bring this work over to main"* → removes secondary, checks out branch in root
- **`/worktree-where`** — *"where am I"* → which worktree, branch, and cwd this session is pointing at

### The loop

A typical end-to-end flow when working on any portfolio repo:

```
mcp__dossier__task_create        # plan: discrete shippable unit
       │
       ▼
/worktree-add <branch>           # isolate: own branch + dir under .claude/worktrees/
       │
       ▼
(write the spec doc inside the worktree, commit, push)
       │
       ▼
mcp__ship__ship { workdir, docPath, repo, branch }    # dispatch cursor against the spec
       │     │
       │     └─ /work-driver coordinates the rest if multiple streams:
       │        poll → land → PR → review cycles → merge → cleanup
       ▼
gh pr create + request reviewers (@copilot + @codex + @claude + @cursor)
       │
       ▼
gh pr merge --squash --admin --delete-branch     # remote-only delete
       │
       ▼
mcp__dossier__task_complete + mcp__dossier__artifact_link { kind: "commit", ref }
       │
       ▼
/worktree-remove                                  # local cleanup (or /worktree-transfer to drain into root)
       │
       ▼
/shipped                          # recap what landed, what's open, what's next
```

Steps 3-7 of this loop are exactly what `/work-driver` automates when you fan multiple streams in parallel. `/status` swaps in for `/shipped` when you need the recap mid-flight rather than post-merge.

### Why this shape

Each layer is independently swappable. Dossier could be Linear or GitHub Projects — it owns "what needs doing." The `/worktree-*` skills could be hand-rolled `git worktree` calls or a Codespace driver — they own "where work happens." Ship could be a different agent runner (Claude Code SDK, a local cursor subprocess, etc.) — it owns "drive an agent against a workdir + persist what happened." Huddle owns multi-seat coordination channels; playwright owns browser. Substituting any one doesn't ripple into the others.

Not every flow uses every tool. A one-off CLI fix can skip dossier; an existing-checkout edit can skip the worktree skills; a non-agent change skips ship. The workbench is a menu, not a checklist — but when the signals above match, default to calling the verb without checking in first.
<!-- END dev-workbench -->

## Architecture

Strict layered dependency direction (mirrored from tower):

```
domain → store → server → bin
```

- `src/domain.rs` — plain types + status enums. No I/O. 1:1 with PROTOCOL.md primitives.
- `src/store.rs` — `FsStore` reads and writes the on-disk corpus per LAYOUT.md.
- `src/server.rs` — `MeshService`: MCP server wrapping the store; tools registered via `rmcp`'s `#[tool_router(server_handler)]`.
- `src/bin/dossier.rs` — CLI entry, stdio transport, arg parsing.

Don't introduce a downward import. If a feature needs a new dependency
direction, lift the shared concern into `domain`.

## Docs

- [docs/vision.md](docs/vision.md) — what dossier is, why, what we're explicitly NOT building. Read first.
- [PROTOCOL.md](PROTOCOL.md) — data model: primitives, verbs, task state machine.
- [LAYOUT.md](LAYOUT.md) — on-disk corpus convention: directory tree, frontmatter shape, append-only `artifacts.jsonl`, concurrency assumptions.
- [docs/features/&lt;feature&gt;/spec.md](docs/features/) — design spec per feature (problem, scope, decisions, acceptance).
- [docs/features/&lt;feature&gt;/plan.md](docs/features/) — execution plan with phase checkboxes when the feature spans multiple PRs (optional).
- [docs/follow-ups.md](docs/follow-ups.md) — cross-cutting backlog from review / implementation passes.

Specs and plans live under `docs/features/<feature>/`. Reference docs go directly under `docs/`. If you change behavior, update the spec in the same PR.

## Develop

```sh
make check        # fmt-check + clippy --all-targets --all-features -- -D warnings + test
make fmt          # apply rustfmt
make lint         # clippy strict (no fix)
make test         # cargo test
make build        # debug build
make release      # release build
```

`make check` is the single command CI runs and the one to run before you
push. Same matrix locally and in CI so failures reproduce. CI: `.github/workflows/ci.yml`.

### Testing techniques

Property tests (proptest) live in `tests/proptest_*.rs` and run as part of
`cargo test`. Default 256 cases; override with `PROPTEST_CASES=N` for thorough
audits.

Mutation testing (cargo-mutants) is opt-in via `make mutants` or `make
mutants-quick`. Not gated in CI — used as an audit tool to find blind spots
in the test suite, not a regression gate.

### Toolchain

- Rust stable, currently 1.95+, via rustup.
- **Windows**: requires Visual Studio Build Tools with the C++ workload.
  `winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`.
  The MSVC toolchain is what `chrono` (and most Windows-aware crates)
  expect. The GNU toolchain rustup ships works for many crates but lacks
  the binutils chrono needs to generate kernel32 import libs.
- **macOS / Linux**: `rustup default stable` is enough.

## Lint discipline

Cheney-flavored strict, mirroring tower's `.golangci.yml` posture:

- `clippy::all`, `pedantic`, `nursery`, `cargo` all warn-by-default
- Selective restriction: no `panic!`, `unwrap`, `indexing_slicing`,
  `dbg!`, `print_stdout`, `todo!`, `unimplemented!` in non-test code
- `unsafe_code = forbid`; `unreachable_pub`, `unused_lifetimes`,
  `unused_qualifications`, `non_ascii_idents` warn
- Complexity caps in `clippy.toml`: cognitive 20, lines 100, args 6
- CI fails on any warning (`-D warnings`)

Don't add `#[allow(...)]` without a one-line justification comment
saying *why*. The few that exist now (`needless_pass_by_value` on
`internal()`, the panicky-lints allow on test modules) all explain
themselves.

`missing_docs` is intentionally deferred until the public API stabilizes
past v0 — most public types are DTOs that are 1:1 with PROTOCOL.md, and
per-field docs would just duplicate the spec.

## Conventions

- **Errors-not-capitalized** (Go convention; matches the rest of the
  portfolio). `bail!("not a dossier corpus...")` not `bail!("Not a...")`.
- **Append-only `artifacts.jsonl`** — never rewrite; future write side
  will use `O_APPEND` + a file lock for the duration of the append.
- **Atomic writes** for files (`.tmp` + rename) when the write side
  lands.
- **Single-writer per corpus** — multi-mesh coordination is out of scope
  for v0; documented in LAYOUT.md.
- **Test modules** gate the panicky lints with `#![allow(...)]` at the
  top of `mod tests`.
- **No design-doc or phase refs in code comments** — doc comments
  describe behavior; roadmap context belongs in commit messages and the
  spec docs.

## Dogfood corpus

dossier tracks itself in `projects/dossier/`. The pattern:

- Phases for major chunks of work (`01-protocol-spec.md`, `02-storage-layout.md`, `03-mesh-skeleton.md`, `04-ship-integration.md`)
- Tasks for the discrete units inside each phase, with status, assignee, and an append-only Notes section
- `artifacts.jsonl` records every commit / file / PR linked to a task

Today these are updated by hand. Once the write side ships, the mesh
itself drives them — that double-loop is what makes the spec real.

## How dossier fits

dossier is the portfolio's project-state coordination plane:

- `../ship` will become the canonical implementer agent — speaks APP via MCP, claims tasks, posts progress, links PRs.
- `../cortex` queries dossier for project context to feed code agents.
- `../tower` observes worktrees; could correlate worktree → project id.
- `../orchestra` runs reference project IDs when its consumers want them tracked.

The mesh doesn't depend on any of these. They depend on the protocol.

## Shipping features

Adapted from ship's workflow:

- Write a design doc under `docs/<feature>/spec.md` — what + why + acceptance criteria + scope.
- Create a branch (e.g. `write-side/projects`, `write-side/tasks-state-machine`).
- Implement.
- Open a PR.
- Request reviewers — copilot, comment `@codex review`, comment `@claude review`.
- Ensure CI is green (`make check` matrix).
- Address review comments; opinionated is fine, don't take comments blindly.
- Repeat the review cycle ~3 times before reaching out.
- Merge when ready.

If a phase has more than ~3–4 distinct steps, treat each step (or small
group) as its own PR — not as substeps inside one mega-PR. Reviewers
should flag a wrong-shape budget at design time, not after a 1500-LOC
PR is open.

## PR sizing

Same bands as ship:

| Band    | Limit (weighted LOC) |
| ------- | -------------------- |
| amazing | < 500                |
| ideal   | < 700                |
| stretch | < 1000               |

Weights:

- production source (incl. doc comments): **1.0×**
- tests + fixtures: **0.5×**
- lockfiles, generated, configs (`Cargo.toml`, `rustfmt.toml`, etc.), docs: **0×**

A design doc declares the budget in its **Scope** section near the top.
If the budget exceeds 700, the doc must either split into multiple
phase docs OR justify the no-split inline (tightly coupled state
machine, single schema you can't ship half of, etc.).

## Common gotchas

- **Corpus marker required** — `FsStore::open` errors if the directory
  doesn't contain `.dossier/`. Not a magic walk-up; pass the right path.
- **ULID format strict** — `prj_` / `phs_` / `tsk_` / `art_` prefix +
  26 chars Crockford base32 (no `I`, `L`, `O`, `U`). Don't hand-write
  invalid ones.
- **`rmcp` 1.6 macro syntax** — `#[tool_router(server_handler)]` is the
  current form. The bare `#[tool_router]` works but doesn't generate a
  `ServerHandler` impl, so the service won't `serve()` without a manual
  impl.
- **Tool return types need `Json<T>` wrapper** for structured output.
  Bare `Result<MyStruct, ErrorData>` fails the `IntoToolRoute` bound.
  Use `Result<Json<MyStruct>, ErrorData>`.
- **Server info reports `name: "rmcp"`** instead of `"dossier"` — the
  `tool_router(server_handler)` macro generates a default `get_info()`
  that doesn't take the `Implementation` from us. Cosmetic; fixed by
  hand-overriding `get_info()` in a follow-up.
- **Schemars version** must align with `rmcp`'s transitive — both 1.x.
  If you see "two versions of schemars in the dep graph," check whether
  a chrono / time / uuid feature is pulling 0.8.

## When you're stuck

- "Clippy warning I don't understand" → run `make lint` locally to see
  it in context. Most are real; the suggestion is usually right.
- "I want to add a write verb that mutates without going through the
  state machine" → stop. The state machine is the protocol. Add an
  enforcement test if you can't see why.
- "I want to add a feature that pulls dossier toward generic doc
  storage / Notion / RAG" → stop. dossier is opinionated PM. Generic doc
  store is explicitly out of scope; see PROTOCOL.md "Out of scope (v0)".
- "Tests pass on Linux, fail on Windows" → most likely a path
  separator (`/` vs `\`) or line-ending (CRLF) assumption. Use
  `Path::join` and `lines()` rather than splitting on `\n`.

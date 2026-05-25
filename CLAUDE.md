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

## Development workbench

Three MCP tools are available in any Claude session on this machine. **This is dossier — the project-memory plane itself** — so the dossier verbs are the most directly relevant when working in this repo, but tower (worktrees) and ship (workflow execution) are part of the same workbench. When the signal matches, **just call the verb**. Don't ask permission.

Dogfood reality: we track dossier's own work in the real corpus at `~/pers/dossier-state/projects/dossier/` (separate from the in-repo test fixture under `projects/dossier/`). When you call `phase.add` / `task.create` for dossier work, it lands there.

### dossier — project memory

Long-term home for what's planned, in-flight, and shipped. Projects → phases (design docs) → tasks → artifacts (PRs / commits / files).

Use proactively for:

- *"What's the state of `<project>`?"* → `project.get { slug }`, then `phase.list` + `task.list { project: <slug>, status: in_progress }`.
- *"I'm starting `<new chunk of work>`."* → `phase.add { project, slug, title, body }`.
- *"I need to do X"* / discrete actionable surfaces → `task.create { project, phase?, slug, title, body }` (status defaults to `todo`).
- User picks up a task → `task.claim { id, actor: human:michael }`. Re-claim by same actor is a no-op.
- Progress / state transition on a task → `task.update { id, status?, note?, ... }`. Append notes liberally — the corpus *is* the working log.
- Open / merged PR, commit ties to a task → `artifact.link { project, task?, kind, ref, label }` without being asked.
- *"Done with task X."* → `task.complete { id, note? }`.

Don't use for:

- Code-level work (write the code first; *then* `artifact.link` the PR).
- Anything that lives only in this session's scratch context.

### tower — workspace substrate

Owns repo registration + git worktrees. Each agent / human task gets its own isolated workspace so the main checkout stays clean and parallel work doesn't collide.

Use proactively for:

- *"Starting `<branch / feature>`."* → `tower.add_worktree { repo, name }`. Branch becomes `tower/<name>`; path becomes `<repo>/.worktrees/<name>`.
- New repo Tower doesn't know yet → `tower.register_repo { path }` (defaults name to dir basename).
- *"What worktrees are open?"* → `tower.list_worktrees { repo? }` — includes PR / CI / review state.
- Worktree done + PR merged → `tower.remove_worktree { repo, name }` to clean up the local clone.

Don't use for:

- Editing files inside the worktree (that's a normal file edit, not a Tower call).
- Anything in the main checkout — Tower's job is the *isolated* workspace.

### ship — workflow execution

Hands a task doc to a coding agent, persists what happened, lets you inspect / cancel / replay. Owns nothing about the workspace (Tower's job) or the planning (Dossier's job).

Use proactively for:

- *"Ship `<task doc>` against `<worktree>`."* → `ship.ship { workdir, docPath, repo, branch }`. V1 blocks until terminal; if the MCP request times out, the workflow continues durably — poll `ship.get_workflow_run { workflowRunId }`.
- *"What ran on `<repo>` recently?"* / *"What's still in flight?"* → `ship.list_workflow_runs { repo?, status?, limit? }`.
- *"What did `<wf id>` do?"* → `ship.get_workflow_run { workflowRunId }` (also accessible via the `ship://runs/{id}` resource).
- An in-flight run needs to stop → `ship.cancel_workflow_run { workflowRunId }` (idempotent on terminal rows).

Don't use for:

- Creating the worktree (Tower).
- Writing the task doc (a normal file edit inside the worktree).
- Recording the result back to project state (Dossier `artifact.link`).

### The loop

A feature on dossier (or any repo in the portfolio) flows through these steps. Steps 3-4 are *aspirational* for dossier itself — we've been driving manually rather than through `ship.ship` so far; the loop documents the pattern, not the historical reality.

1. **Dossier** — `project.create` (once per feature), `phase.add` (one per stage), `task.create` (one per shippable unit).
2. **Tower** — `tower.register_repo` (once per repo), then `tower.add_worktree` per task. Branch = `tower/<name>`, path = `<repo>/.worktrees/<name>`. *Optional for small / single-session dossier PRs; required when running things in parallel.*
3. **Spec doc** — write `docs/features/<feature>/spec.md` with dossier's standard header (`**Status / Owner / Date / Related**` block at the top, then `## Scope` with weighted LOC estimate, then `## Goal`, then behavior + acceptance + testing). Multi-PR features get an additional `docs/features/<feature>/plan.md` with phase checkboxes. Commit + push.
4. **Implement** — either `ship.ship` against the worktree + spec (when the work fits an agentic run), or drive manually in a Claude Code session (current default for dossier). Both end in a working branch with the change.
5. **PR** — push, open, request reviewers (Copilot via `gh pr edit --add-reviewer copilot-pull-request-reviewer`; `@codex review`; `@claude review`).
6. **Dossier (close)** — `task.complete { id, note }` + `artifact.link` to bind the merged PR url back to the task.

### Why three MCPs and not one

Each layer is independently swappable. Dossier could be GitHub Projects, Linear, or a Notion DB — it owns "what needs doing." Tower could be plain `git worktree`, a cloud Cursor agent ref, or a Codespace — it owns "where work happens." Ship owns "drive an agent against a workdir + persist what happened" and only that. Substituting any one doesn't ripple into the other two.

Not every flow uses all three. A one-off CLI fix can skip Dossier; an existing-checkout edit can skip Tower; a non-agent change skips Ship. The workbench is a menu, not a checklist — but when the signals above match, default to calling the verb without checking in first.

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

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
make check        # fmt-check + clippy --all-targets -- -D warnings + test
make fmt          # apply rustfmt
make lint         # clippy strict (no fix)
make test         # cargo test
make build        # debug build
make release      # release build
```

`make check` is the single command CI runs and the one to run before you
push. Same matrix locally and in CI so failures reproduce. CI: `.github/workflows/ci.yml`.

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

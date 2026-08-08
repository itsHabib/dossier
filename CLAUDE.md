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
get / overview / create / update`, `phase.list / add / update`,
`task.list / get / create / claim / update / complete`,
`artifact.list / link`, and `search`. The state machine is
runtime-guarded, atomic writes route through helpers, slug validation
is enforced on every create path. List verbs are live-by-default (see
[docs/vision.md](docs/vision.md)).

The **verdict/receipt substrate** (state-substrate TDD, Phase A) is live:
`Artifact` carries an optional flat `meta` map (caps enforced, immutable
per `(task, kind, ref)` — correct via supersede, not mutation);
`verdict` and `receipt` are well-known `kind`s with documented canonical
`ref` forms and `meta`-key conventions (PROTOCOL.md); `artifact.list`
filters on `ref`. This is the substrate `/wip`, `/shipped`, and flare
join against instead of each reaching into gate's audit chain or ship's
ledger directly — see PROTOCOL.md's "Artifact kind conventions" and
LAYOUT.md's `artifacts.jsonl` section for the exact shapes. Emitting is
still by convention (skills call `artifact.link` at close-out);
auto-wiring the driver loop to write these automatically is a separate,
not-yet-built initiative (Phase D of the TDD).

What's not here yet: auto-wiring writes into the ship/driver loop (so
merges/close-outs update the corpus without a human remembering to);
richer retrieval (indexing the `## Notes` journal, a cross-corpus
recap verb); and conflict detection / multi-writer verbs. The S3
backend (`s3-cloud` phase) exists but multi-writer stays parked until a
real team workflow needs it — single-writer local use is the priority.
These land when a real workflow needs them, in the right order (see
[docs/vision.md](docs/vision.md)).

<!-- BEGIN dev-workbench (managed by /dev-workbench skill - re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

These MCPs, planes, and skills are available in Claude and Codex sessions on this machine; each harness injects tool signatures, so this is the map of how they compose, not a second verb manual. When the signal matches, call the verb. Knowledge questions about another portfolio repo go to `/consult`; authority questions - direction, spend, credentials, irreversible actions - go to the operator.

**MCPs (in-session):**
- **dossier** - durable project memory: projects -> phases -> tasks -> artifacts.
- **ship** - dispatch an agent and persist dispatch -> poll -> judgment -> land -> record.
- **huddle** - optional multi-seat coordination; off the normal PR path.
- **playwright** - browser automation when the task requires a real DOM.

**Planes (workbench CLIs composed through exit codes and JSONL, not MCPs):**
- **gate** - authorization at the exact PR head against an operator-minted grant; findings are not authorization. Exit 0 pass / 1 block / 2 park / 3 refuse / 4 error.
- **flare** - best-effort notification sink over authoritative receipts; never gates.
- **console** - read-only local view of Gate's inbox and grant ledger; explains, never decides.
- **escalate** - agent -> human -> agent resolution channel for a parked Gate run.

**Skills:**
- **/work-driver** + **/work-driver-prep** - drive implementation; prep builds specs and conflict-safe batches.
- **/pr-risk** - decide how much review a change needs; reviewers perform it.
- **/review-coordinator** + **/review-digest** - consolidate reviewer findings; digest is the deterministic pre-pass.
- **/shipped** / **/status** / **/wip** - retrospective / current-session / portfolio-liveness views.
- **/consult** - ask a sibling repo's steward; knowledge to peers, authority to the operator.
- **/worktree-*** - add / list / remove / transfer / locate isolated checkouts.

### The loop

```text
dossier task -> /worktree-add -> spec -> ship driver (dispatch -> poll -> judgment -> land -> record)
  -> PR + CI -> /pr-risk -> reviewer panel -> /review-coordinator -> one findings artifact
  -> gate evaluates the exact head -> 0: emitted head-pinned merge command -> merge
  -> authoritative receipts -> dossier close-out -> /worktree-remove
       \-> 2: park -> console / gate next -> human decision -> escalate -> gate resolve -> gate next
       \-> attention or terminal receipt -> flare -> Slack (best effort; never gates)
```

### Why this shape

Each layer owns one responsibility and can be replaced without rippling: dossier owns what needs doing; worktree skills own where work happens; Ship owns agent execution and durable run state; pr-risk owns review depth; reviewer bots are swappable finders; review-coordinator owns their consolidated artifact; Gate alone owns exact-head merge authorization; Escalate carries a human resolution without deciding it; Console explains Gate state; Flare notifies; Consult handles cross-repo knowledge. The workbench is a menu, not a checklist.

### The shape underneath

The contract planes are **State** (dossier plus run, verdict, grant, and receipt artifacts), **Execution** (Ship), **Verification** (review and Gate's escalate-only verifier ladder), **Capability** (scoped operator-minted grants), and **Observability** (Console, Flare, /wip, /shipped, /status). This section is **Composition**. Planes share typed artifacts - evidence -> verdict -> action - rather than call stacks.
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

- [docs/vision.md](docs/vision.md) — what dossier is, why, and how it sequences what to build next. Read first.
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

<!-- BEGIN eng-philo (managed by /eng-philo — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Engineering principles

How code is written here — Dave Cheney lineage ([Practical Go](https://dave.cheney.net/practical-go)): simplicity, clarity, line-of-sight. Apply on every change; the lint below catches the slips.

1. **No `else` — line-of-sight.** Handle errors / edge cases with early returns and guard clauses; keep the happy path un-indented, flowing down the left margin. Reaching for `else` → return early instead.
2. **Shallow nesting — ≤2 levels *per scope*.** A `for` + an `if` is the ceiling in one scope. The budget is per-scope, not per-function — a closure / anon fn is its own scope, so a `for`+`if` inside a closure is fine. Deeper in one scope → extract a function.
3. **Policy vs mechanism.** Separate the decisions (policy: validation, state machines, business rules) from the plumbing (mechanism: persistence, transport, I/O). Mechanism is dumb and swappable; policy lives in a layer above it. Never let policy leak into a mechanism layer.
4. **Composition of single-responsibility layers.** Each layer / package owns ~one responsibility; the app is a *composition* of them; any piece is swappable without rippling into the others. Dependencies flow one direction.
5. **Small, sharp APIs.** Export the least callers need. Intention-revealing names. Accept the narrowest input, return concrete types. Make the zero value useful.
6. **Errors are values; simplicity over cleverness.** Handle or propagate errors explicitly — never swallow. Readable > clever > short. A little copying beats a premature abstraction or dependency.

### Rust idioms + enforcement

`?` over nested `match`; early-return guards, no `else` after a `return`; newtypes for domain values; minimal surface (lean on `pub(crate)`, `unreachable_pub`).

*Enforce:* clippy `cognitive_complexity` + `too_many_lines`, `clippy.toml` complexity caps, `-D warnings`.
<!-- END eng-philo -->

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
  storage / Notion / RAG" → pause and check the fit. dossier is
  opinionated project memory; a generic doc store is a different shape,
  and semantic retrieval composes better in the LLM that queries the
  corpus (see PROTOCOL.md "Not in v0"). If a real workflow keeps needing
  it here, that's the signal to design it in — deliberately, not by
  default.
- "Tests pass on Linux, fail on Windows" → most likely a path
  separator (`/` vs `\`) or line-ending (CRLF) assumption. Use
  `Path::join` and `lines()` rather than splitting on `\n`.

<!-- local-offload:start -->
## Local-first offload

Before spending cloud tokens on a mechanical sub-step, check for a free local path (needs the `local` CLI / Ollama on this machine):

- Narrowing a big file list, extracting structure from noisy tool output, shallow classification -> `/offload`
- "Have we solved/decided this before?" questions about the operator's own work -> `/ask-portfolio`
- Triaging a PR's bot-comment pile -> `/review-digest <PR#>`

Deep judgment (code review, risk calls, dense-diff reasoning) stays with the primary model. If `local` is not on PATH, skip silently -- never block on this.
<!-- local-offload:end -->

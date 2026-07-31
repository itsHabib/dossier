# Dossier

Dossier is an opinionated project-memory MCP server over a markdown-on-disk
corpus. The corpus is authoritative; the server is a typed API over it. Read
[docs/vision.md](docs/vision.md), [PROTOCOL.md](PROTOCOL.md), and
[LAYOUT.md](LAYOUT.md) before changing behavior.

## State

The write side is shipped for project, phase, task, artifact, overview, and
search operations. State transitions, slug validation, atomic file writes, and
artifact metadata immutability are runtime-enforced. `verdict` and `receipt`
artifacts form the substrate joined by `/wip`, `/shipped`, and notification
surfaces. Driver auto-emission and multi-writer coordination remain separate
future work; do not imply they are live.

## Architecture

Dependencies flow one way:

`domain → store (FsStore | S3Store) → server/ → bin`

- `src/domain.rs` owns protocol types and state enums.
- `src/store.rs` and `src/s3store.rs` own persistence mechanisms.
- `src/server/` owns the MCP service and tool routing.
- `src/bin/dossier.rs` owns CLI arguments and transport.

Do not pull policy downward or turn Dossier into generic document storage.

<!-- BEGIN dev-workbench (managed by /dev-workbench skill — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

The harness injects individual tool signatures; this is the composition map.
Call the matching verb when its signal appears. Ask sibling stewards knowledge
questions through `/consult`; reserve direction, spend, and irreversible choices
for the operator. **This repository is the durable work-item State store.**

**Roster:** dossier owns durable work state; ship drives and records agent runs;
gate authorizes an exact PR head; console shows gate's inbox; escalate carries a
human decision back to gate; flare sends best-effort notifications; `/pr-risk`
sizes review; `/review-coordinator` consolidates findings; `/worktree-*` owns
isolation; huddle and browser automation are optional supporting tools.

### Loop

`dossier task → worktree/spec → ship driver → PR + CI → pr-risk → reviewers →
review-coordinator → gate exact-head authorization → merge → receipts → dossier
close-out`, with parks surfaced by console and resolved through escalate. Flare
observes receipts independently and never gates.

### Seams

State, execution, verification, capability, and observability communicate through
typed artifacts—not call stacks. Findings do not authorize; gate alone owns the
merge boundary. Console explains but never decides, escalate transports but never
chooses, and flare notifies but never blocks. The workbench is a menu, not a
checklist.
<!-- END dev-workbench -->

## Develop

```sh
make check        # fmt-check + strict clippy + tests
make fmt
make lint
make test
make mutants      # audit only, never a merge gate
```

Property tests live under `tests/` and run with `cargo test`. Rust stable on
Windows requires the MSVC C++ build tools; macOS/Linux need standard rustup.

## Invariants

- `unsafe_code` is forbidden; clippy warnings fail CI.
- Do not add `#[allow]` without a one-line reason.
- Errors are lowercase and contextual.
- `artifacts.jsonl` is append-only; correct metadata through supersession.
- Files use temp-write plus rename; the local corpus is single-writer.
- Strict ULIDs use the documented prefix and Crockford alphabet.
- Structured tool responses use `Json<T>`.
- `schemars` must remain aligned with `rmcp`.
- Code comments describe behavior, not design phases.

<!-- BEGIN eng-philo (managed by /eng-philo — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Engineering principles

Keep the happy path on the left: guard and return instead of `else`, and extract
before nesting exceeds two levels in one scope. Separate policy from mechanism;
compose single-purpose layers with one-way dependencies; expose small,
intention-revealing APIs; handle and wrap errors explicitly. Prefer readable
copying over premature abstraction.

Enforcement: clippy complexity caps, minimal visibility checks, and `-D warnings`
through `make check`.
<!-- END eng-philo -->

## Delivery

Behavior changes update their spec in the same PR. Use
`docs/features/<feature>/spec.md` and optional `plan.md`; Dossier does not use
Ship's `phases/<NN>-<slug>` convention. Follow the configured review panel and
global exact-head Gate flow.

Weighted PR guidance: production 1×, tests/fixtures 0.5×, docs/config/generated
0×. Aim below 700 weighted lines; split or justify larger changes in the spec.

The dogfood corpus under `projects/dossier/` is maintained through the shipped
write verbs; do not retain guidance claiming the write side is pending.

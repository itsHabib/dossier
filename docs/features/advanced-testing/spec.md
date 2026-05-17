**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: [vision.md](../../vision.md), [PROTOCOL.md](../../../PROTOCOL.md), [LAYOUT.md](../../../LAYOUT.md), [filter-expansion/spec.md](../filter-expansion/spec.md)

# Advanced testing — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | none — testing-only feature | 0 | 0 |
| Tests | `tests/proptest_state_machine.rs`, `tests/proptest_slug_roundtrip.rs`, `tests/proptest_frontmatter_roundtrip.rs`, helpers in `tests/common/` | ~550 | 275 |
| Configs / dev-deps | `Cargo.toml` dev-deps, `.cargo/mutants.toml`, `Makefile` targets, `.github/workflows/ci.yml` (optional gating) | ~60 | 0 |
| Docs | this spec, README mention, `CLAUDE.md` paragraph | ~150 | 0 |
| **Total** | | | **~275** |

Band: **amazing** (<500 weighted). Split into two PRs by technique (property tests first, mutation testing second). Each lands independently — neither blocks the other.

## Goal

Raise the *quality* of dossier's existing test suite without adding production code. Today's tests are good at confirming the happy path on hand-picked inputs; they are weaker at exploring the input space and at telling us when a test would still pass after the code under test is silently broken. Property testing addresses the first weakness; mutation testing surfaces the second.

The corpus markdown format, the task state machine, and the slug rules are all places where *one wrong character* changes correctness. They are exactly the kind of code that property + mutation testing are designed to harden. Adding both is a one-time investment that compounds for every future verb.

## Background — what these techniques are (and why now)

If you have not used these before, this section is the orientation. Skip if you already know.

### Property testing — "for *any* input matching X, the code must satisfy Y"

A normal unit test asserts on **one** example: *"given a task in `Todo`, calling `claim_task` returns a task in `Claimed`."* It catches the bug only if the example happens to expose it.

A property test asserts on a **universally-quantified statement**: *"for any task in `Todo`, calling `claim_task` returns a task in `Claimed` with `assignee` set."* The test framework (`proptest`) generates dozens or hundreds of random `Todo` tasks each run and checks the property on every one. When a generated input fails the property, the framework then *shrinks* — repeatedly removes complexity from the failing input until it finds the smallest example that still fails. You get back a minimal counter-example: *"a task with assignee=`` and notes=`[]` and status=`Todo` violates the property."*

What this catches that example tests miss:

- **Edge cases in the input space you didn't think of.** `body_contains: ""` (empty string) — does it return everything, nothing, or error? Most hand-written tests use `"auth"`; proptest will try the empty string, a single space, a string containing `\0`, a 10 KB string of Unicode combining characters.
- **Algebraic relationships between operations.** *"`serialize` then `deserialize` should equal the original for any valid Task"* is a property — not a single example. If frontmatter serialization loses data on round-trip for any task with notes containing a tab character, proptest will find it; a hand-written test that picks `"some note"` will not.
- **State machine invariants.** *"After any legal sequence of operations on a task, terminal states (`Done`, `Cancelled`) never transition out."* This is checked by generating random sequences of operations, replaying them against the real store, and asserting the invariant holds at every step. Manually writing a test for every possible sequence is exponential; proptest covers the interesting ones in seconds.

Cost: a property test takes ~2-3× longer to *write* than an example test (you have to think about the universal claim and write a generator), but each one replaces dozens of hand-rolled cases. It also takes longer to run (~100ms-2s per property, vs <1ms for an example), so it lives in the regular test suite but doesn't bloat it unreasonably.

### Mutation testing — "would this test catch the bug if I introduced one?"

Tests measure code; **mutation testing measures the tests themselves.** The tool (`cargo-mutants`) makes small, automated edits to the source — flip a `==` to `!=`, change a `<` to `<=`, swap `&&` for `||`, replace a function body with `Default::default()`, etc. — then runs the test suite against each mutated version of the code.

If the tests still pass after the mutation, the mutation **survived** — which means the tests didn't actually exercise that line meaningfully. Surviving mutations are where the test suite has blind spots. If the tests fail, the mutation was **caught** — the suite genuinely exercises that behavior.

A worked example for dossier: in `validate_task_update_transition`, the line `if matches!(from, TaskStatus::Done | TaskStatus::Cancelled)` rejects every transition out of a terminal state. A mutation might flip this to `if matches!(from, TaskStatus::Done)`. If the test suite has no test that tries to transition out of `Cancelled` specifically, the mutation survives — and we know we're under-testing one branch.

Mutation testing answers a question that coverage doesn't: *"is my test for this line actually checking the right thing?"* 100% line coverage with mutation survival ≈ 50% means tests are visiting code but not asserting on its effects.

Cost: high per run (we re-run the full test suite once per mutation, and there are typically a few hundred mutations in a small Rust crate — 30-60 minutes on dossier's current size). Cheap per insight. It is **not** something you run on every commit; it is something you run occasionally to find blind spots, fix them, and move on. Think "audit," not "gate."

### Why this project benefits — concretely

| Property | Where it lives | Why it's a good target |
|---|---|---|
| **Task state machine invariants** | [src/store.rs:1085](../../../src/store.rs) `validate_task_update_transition` + `claim_task` + `complete_task` | Six statuses × four entry points = many small boolean branches. Off-by-one-status bugs are the kind of thing only random sequences find. |
| **Slug validation round-trip** | [src/store.rs:1235](../../../src/store.rs) `is_valid_slug` | Pure function on `&str`; the legal-character predicate is the property. |
| **ULID prefix round-trip** | [src/store.rs:1222](../../../src/store.rs) `new_id` | "For any prefix, output parses back as `prefix_<ulid>` and the ulid portion is a valid ULID." |
| **Frontmatter round-trip** | `serialize_*_file` / `read_frontmatter` / `load_task` etc. | The cross-platform CRLF / LF gotcha called out in CLAUDE.md is exactly the kind of bug a round-trip property catches. |
| **`split_task_body` boundary** | [src/store.rs:331](../../../src/store.rs) | The `## Notes` heading is a load-bearing delimiter. `validate_task_body` rejects it; property-test that *any* string `validate_task_body` accepts round-trips cleanly through `split_task_body`. |
| **`body_contains` predicate** | `FsStore::list_*` | The literal-substring contract is easily property-tested: result includes task ⇔ task's body (lowercased) contains the query (lowercased). |

Mutation testing on top of those tests then tells us, per branch, whether the property is *strong enough* — a property like "claim returns Ok" survives the mutation that drops `task.claimed_at = Some(now)` because nothing checks the timestamp. The mutation report tells us to tighten the assertion.

## Behavior

### Add `proptest` as a dev-dependency

```toml
[dev-dependencies]
proptest = "1"
```

No production dependency. `proptest` lives entirely under `[dev-dependencies]` and adds no runtime cost or binary weight. Property tests live in `tests/` (integration test crate) so they can use the same `tempfile`-based corpus harness the existing integration tests use; per-module unit-style proptests would also work but keeping them out of `src/` keeps the lint surface clean.

### Add property tests for the six targets above

One integration-test file per target:

- `tests/proptest_state_machine.rs` — model-based test. The model is an enum mirroring the legal MCP verbs (`Claim { actor }`, `UpdateStatus { to }`, `Complete`, `UpdateBody { body }`). Proptest generates a `Vec<Operation>`; the test replays it against a real `FsStore` and after each step asserts the invariants below.
- `tests/proptest_slug_roundtrip.rs` — generators for "valid slug" (regex-bounded) and "invalid slug" (arbitrary `String`, filtered to fail `is_valid_slug`). Properties: valid slugs round-trip; invalid slugs are rejected by `project.create` / `phase.add` / `task.create`.
- `tests/proptest_frontmatter_roundtrip.rs` — generators for `Project`, `Phase`, `Task`. Property: `serialize → write → read → parse` yields a structurally-equal value (modulo whitespace normalization documented in LAYOUT.md).
- The body-split, `body_contains`, and ULID properties are small enough to ride along inside `proptest_frontmatter_roundtrip.rs` and `proptest_state_machine.rs` rather than getting their own files.

Invariants the state-machine test asserts after every step:

1. **Terminal absorption.** If `task.status ∈ {Done, Cancelled}` at step *N*, then for all subsequent steps the status is unchanged and `completed_at` (for Done) is unchanged.
2. **Assignee/status coupling.** After any step, either `status == Todo ∧ assignee == ""`, or `status ∈ {Claimed, InProgress, Blocked} ∧ assignee != ""`, or `status ∈ {Done, Cancelled}`. The "corrupt" combinations (`Todo` with assignee, or non-`Todo` without assignee) never appear.
3. **Idempotent re-claim.** Two consecutive `Claim { actor: a }` operations yield the same final state (apart from monotonically-non-decreasing `updated_at`).
4. **`update_task` cannot reach `Claimed` or `Done`.** Generating those targets always errors; the file on disk is unchanged.
5. **`complete_task` requires `InProgress`.** From any other status, errors.
6. **Timestamp monotonicity.** `updated_at` never goes backwards across legal operations.

### Add `cargo-mutants` as an opt-in audit

`cargo-mutants` is a separate binary, not a crate dependency. Install via `cargo install cargo-mutants`. The config file (`.cargo/mutants.toml`) declares which paths to mutate and which to skip:

```toml
# Mutate production source only.
examine_globs = ["src/**/*.rs"]
exclude_globs = ["src/bin/**", "tests/**"]

# Skip tiny modules where mutation generates noise without insight.
# (None today — re-evaluate after first run.)

# Use the existing test target; default config is fine.
test_tool = "cargo"
```

Make targets:

```make
mutants:        # full mutation run; slow (~30-60 min)
\tcargo mutants --no-shuffle

mutants-quick:  # only mutate files changed vs main; for PR triage
\tcargo mutants --in-diff <(git diff main..HEAD)
```

Not added to `make check` and not gated in CI in this PR. The output is an HTML / text report under `mutants.out/`; it is read by a human and the actionable findings turn into either new tests or `skip_calls` entries.

## Decisions

- **Two PRs, not one.** Property tests have immediate value and ship in PR 1. Mutation tooling is independent and lands in PR 2. Bundling them risks a 700-LOC PR for two unrelated capabilities. Reviewers can evaluate each on its own merits.
- **No CI gating for mutation testing.** Wall-clock cost is too high to put on every PR. The pattern is: run locally before a release / before a refactor of a sensitive area, file follow-up tickets for surviving mutations worth fixing. Re-evaluate gating after we have a baseline mutation-survival number; if it's already low, gating buys little.
- **Property tests live in `tests/`, not `#[cfg(test)] mod tests` inside `src/`.** Two reasons. First, generators and helpers grow, and a top-level `tests/common/` module is cleaner than scattering them across source files. Second, `tests/` runs against the public crate API only — which is the same surface MCP exercises, so the properties stay honest about the abstraction boundary.
- **Model-based test uses the real `FsStore`, not a mock.** Following the project convention (CLAUDE.md: integration tests hit the real artifact, not mocks) and matching how the existing unit tests work via `tempfile`-backed corpora. The state machine logic lives in store.rs; mocking it out would test the model, not the code.
- **Default `proptest` config: 256 cases per property, 10s timeout.** Standard. Override per property if a generator is particularly expensive. Failing seeds are persisted under `proptest-regressions/` and committed to git so reproductions are deterministic across machines.
- **No QuickCheck.** `proptest` has shrinking, integrated regression replay, and a more ergonomic strategy combinator API. QuickCheck still works in Rust but is the older option; no reason to pick it.
- **No fuzzing (`cargo-fuzz`, `libFuzzer`) in this scope.** Fuzzing targets unsafe / parser-heavy code looking for crashes; dossier has neither. Property + mutation cover the surface we have. Revisit if we ship a non-trivial parser (e.g. a query DSL).

## Non-goals

- **No production-source changes.** This is a testing-quality feature, not a behavioral change. If a property reveals a real bug, *that* bug fix is a separate PR — it would have been a bug regardless of how we found it.
- **No new lints, no new CI required-check.** `make mutants` is opt-in. `cargo test` already runs proptests because they're in `tests/`.
- **No coverage tooling** (`tarpaulin`, `llvm-cov`). Mutation testing supersedes coverage for the question we care about ("are these lines actually tested?"). Adding both is double counting.
- **No fuzzing** (see above).
- **No property tests on the MCP transport layer.** The `rmcp` macros generate that boundary; it's not ours to property-test. The properties target the store and domain logic.

## Acceptance

- `cargo test` passes including the new proptest modules.
- A deliberately-introduced bug in `validate_task_update_transition` (e.g. removing the `Cancelled` branch from the terminal-state check) causes the state-machine proptest to fail with a shrunk counter-example pointing at a `Cancelled → Blocked` (or similar) attempt. Demonstrating this in the PR description is acceptance for PR 1.
- `make mutants` runs to completion locally and produces a `mutants.out/` report. The PR description records the baseline numbers: total mutations, caught, missed, timeout. Acceptance for PR 2.
- At least three concrete surviving mutations from the first `make mutants` run are either (a) fixed by a tightened test in the same PR, or (b) filed as follow-up tickets in [docs/follow-ups.md](../../follow-ups.md) with rationale ("not worth fixing because…").
- CLAUDE.md gets a one-paragraph "Testing techniques in use" section so future agents know property/mutation testing is part of the workflow.

## Test plan

This feature *is* tests, so "test plan" reduces to: how do we know the tests themselves are doing their job?

- **Mutation testing validates the property tests.** Run `make mutants` after PR 1 lands but before PR 2. The number of surviving mutations is the property-test quality signal. If half survive, the properties are too weak; tighten.
- **Manual bug injection.** Before opening PR 1, deliberately introduce 3 small bugs (one per target file: state machine, slug validator, frontmatter serializer) and confirm each is caught by the proptest module. Revert the bugs; document in PR description.
- **Wall-clock budget.** Full `cargo test` runtime should not grow more than +10 seconds with proptests included. If it does, lower per-property case counts on the slow ones.

## Implementation sketch

### PR 1: property tests

1. Add `proptest = "1"` to `[dev-dependencies]`.
2. Add `tests/common/mod.rs` with a `tempdir_corpus()` helper that creates a fresh `.dossier/` corpus in a `tempfile::TempDir` (mirrors helpers in existing `#[cfg(test)] mod tests` in `src/store.rs`).
3. Add `tests/proptest_slug_roundtrip.rs` — start here; smallest, exercises generator syntax, lands fast.
4. Add `tests/proptest_frontmatter_roundtrip.rs` — generators for the three domain types; round-trip property; CRLF/LF edge cases explicit in the generator.
5. Add `tests/proptest_state_machine.rs` — model + sequence generator + invariants. The bigger lift.
6. Commit `proptest-regressions/` directory pattern to `.gitignore`? **No** — we want shrunk regressions checked in so a re-run on another machine replays them. Confirm `proptest-regressions/` lands in git.

### PR 2: mutation testing

1. `cargo install cargo-mutants` (developer-machine step; no project change).
2. Add `.cargo/mutants.toml` per Decisions above.
3. Add `mutants` and `mutants-quick` make targets.
4. Run `make mutants` against a clean tree, capture baseline, fix or file surviving mutations.
5. Update CLAUDE.md "Testing techniques" paragraph.
6. **Do not** add `mutants` to `make check` or CI.

## Open questions

- **How many cases per property is enough?** `proptest`'s default is 256. On a state-machine test that exercises a real filesystem, 256 × 5-step sequences = 1280 file-system operations per run. That's likely too slow if we run it on every test invocation. Open: do we lower to 64 by default and override upward locally / in a nightly job? Decide after measuring.
- **Should we commit `proptest-regressions/`?** Default proptest behavior writes a regression seed file when a property fails so future runs replay the failing input first. Conventional wisdom: commit it. Open: any concern about diff noise? Probably no.
- **Mutation testing in CI eventually?** Not in this scope, but worth naming the criteria. Plausible: nightly job on `main`, posts the diff vs. previous baseline as an issue comment. Defer until we have a baseline to diff against.
- **Coverage of `server.rs`?** The MCP tool layer is mostly thin wrappers over store calls. Property testing the store covers most of the value; mutation testing the wrappers may surface dead defensive code. Open: run mutants over `server.rs` in PR 2 and see what the report says before deciding to write more tests there.
- **`body_contains` Unicode case-folding?** Today's `is_valid_slug` is ASCII-only, but `body_contains` lowercases via `str::to_lowercase` which is Unicode-aware. The property *"row matched ⇔ task.body.to_lowercase().contains(query.to_lowercase())"* is exact, but reviewing edge cases (Turkish dotless i, German ß) might surface a behavior choice worth documenting. Likely a non-issue at v0; flag for later.

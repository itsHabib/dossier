# AGENTS.md

See [CLAUDE.md](CLAUDE.md) for the authoritative project guide (architecture, conventions, lint discipline, develop commands, common gotchas).

## Cursor Cloud specific instructions

- **Toolchain**: The pre-installed Rust 1.83 is too old — `rmcp-macros` requires edition 2024 (Rust 1.85+). Run `rustup default stable && rustup update stable` once at session start to ensure a recent toolchain is active. If you see `feature edition2024 is required`, the default toolchain is stale.
- **No external services**: This is a self-contained Rust binary with no databases, Docker, or network dependencies. `cargo test` creates temp dirs via `tempfile` — no setup needed.
- **Single CI command**: `make check` (= `fmt-check` + `clippy --all-targets --all-features -- -D warnings` + `cargo test --all-features`). Run before every push.
- **Testing the MCP server**: The binary communicates over stdio. Pipe JSON-RPC messages to `./target/debug/dossier --corpus <path> serve`. The in-repo dogfood corpus at the repo root (has `.dossier/` marker) works as a test corpus.
- **CLI subcommands**: `task_list`, `task_complete`, `task_update`, `artifact_link` work against the corpus directly (no MCP). Use `--corpus .` from the repo root to hit the dogfood corpus.
- **Corpus marker**: `FsStore::open` requires a `.dossier/` directory at the corpus root. The repo root already has one.

<!-- BEGIN dev-workbench (managed by /dev-workbench skill - re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

These MCPs, planes, and skills are available in Claude and Codex sessions on this machine; each harness injects tool signatures, so this is the map of how they compose, not a second verb manual. **This is dossier - the State plane work-item store - so dossier verbs are most directly relevant here.** When the signal matches, call the verb. Knowledge questions about another portfolio repo go to `/consult`; authority questions - direction, spend, credentials, irreversible actions - go to the operator.

**MCPs (in-session):**
- **dossier** - durable project memory: projects -> phases -> tasks -> artifacts.
- **ship** - dispatch an agent and persist dispatch -> poll -> judgment -> land -> record.
- **channel** - optional append-only agent message bus (`channel.post/read/list`); off the normal PR path and supersedes huddle.
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

`/work-driver` coordinates dispatch -> poll -> land and runs its own review triage inline. `/pr-risk` and `/review-coordinator` are explicit steps; the driver does not invoke them automatically.

### Why this shape

Each layer owns one responsibility and can be replaced without rippling: dossier owns what needs doing; worktree skills own where work happens; Ship owns agent execution and durable run state; pr-risk owns review depth; reviewer bots are swappable finders; review-coordinator owns their consolidated artifact; Gate alone owns exact-head merge authorization; Escalate carries a human resolution without deciding it; Console explains Gate state; Flare notifies; Consult handles cross-repo knowledge; Channel owns optional agent-to-agent messaging and supersedes Huddle. The workbench is a menu, not a checklist.

### The shape underneath

The contract planes are **State** (dossier plus run, verdict, grant, and receipt artifacts), **Execution** (Ship), **Verification** (review and Gate's escalate-only verifier ladder), **Capability** (scoped operator-minted grants), and **Observability** (Console, Flare, /wip, /shipped, /status). This section is **Composition**. Planes share typed artifacts - evidence -> verdict -> action - rather than call stacks.
<!-- END dev-workbench -->

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

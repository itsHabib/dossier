# State substrate — verdicts & receipts as first-class memory — Technical Design Document

**Status:** draft / proposal — **NOT a build commitment.** The artifact we decide *from*.
**Owner:** @itsHabib
**Date:** 2026-07-23
**Related:** [vision.md](../../vision.md), [PROTOCOL.md](../../../PROTOCOL.md) (Artifact primitive, "Not in v0"), [LAYOUT.md](../../../LAYOUT.md) (`artifacts.jsonl`), CLAUDE.md "Dev workbench" + "The shape underneath" (the five contract planes), dossier project in the dogfood corpus.

> **Reviewers — focus areas:** (1) the pointer-plus-summary vs. authoritative-copy call in §4 D1, (2) the flat `meta` map and its caps in §5, (3) the no-new-verbs surface in §6, (4) the in-scope / out-of-scope line on driver auto-wiring in §1 and §4 D6.

---

## 1. Problem & hypothesis

dossier is being pulled toward one of two identities, and this doc commits to one.

The **wrong** frame is "Jira-lite for agents" — a ticket tracker whose roadmap grows boards, sprints, labels, and every argument becomes "well, Jira has X." The task-tracking surface stays exactly as lean as it is today; that is settled, not up for review here.

The **right** frame — the one CLAUDE.md's workbench map already asserts — is dossier as the **State substrate of the dev workbench**: the memory of *how the portfolio's work actually happened*. Not more surface; more *connective tissue*.

The gap: the workbench already **emits** that memory, but it's scattered. `ship`'s driver produces run records (ship's ledger); `gate` produces authorization verdicts, hash-chained audit artifacts, and grants (`~/pers/gate`); merges produce receipts (GitHub + close-out notes). None of it is linked to the dossier task/phase the work belonged to — so "why did this PR merge?", `/wip`, `/shipped`, and `flare` each join three stores, or just don't answer.

**The bet:** promote `verdict` and `receipt` to well-known artifact kinds, give artifacts a small structured `meta` map, and the existing `artifact.link` / `artifact.list` surface becomes the **one substrate** every retrospective read joins against. The artifact kind is already an extensible free-form string (`src/domain.rs` round-trips unknown kinds untouched), so this is a deepening of the existing primitive, not a new one.

**Scope / budget:** two code PRs — **≤300 wLOC** (meta field + immutability) and **≤200 wLOC** (`ref` filter) — plus a docs-only PR (0×). Both code PRs sit in the amazing band; total well under the 700 ideal ceiling, no split-justification needed.

**Non-goals (this doc):**
- **Jira-surface drift** — no boards, sprints, labels, estimates, swimlanes. Rejected per vision.md; workflow conventions layer on top of the primitives, they don't become primitives.
- **Generic doc / blob store** — verdicts and receipts are dense pointers + summaries, not bodies. Full run logs, review threads, and audit chains stay in their home stores; dossier links.
- **Re-judging** — dossier stores verdicts; it never evaluates, re-scores, or re-authorizes them. Verification logic (gate's reducer, review-coordinator) must not leak into State.
- **Hash-chain custody** — gate's audit chain remains the cryptographic authority. dossier does not re-sign or verify chains.
- **Driver auto-wiring** — making corpus writes a side effect of the ship/driver loop is a named *adjacent initiative*, not this doc (§4 D6).
- **Gate as the required status check** (the "option (c)" fix for the solo-repo `REVIEW_REQUIRED` deadlock) — adjacent context only; it changes *where the verdict-of-record binds in GitHub*, not how dossier stores it (§10 Q2).

## 2. Functional & non-functional requirements

**Functional**
- FR1 — A gate decision, driver run, merge, or grant usage can be linked to a dossier project (and optionally a task) as a typed artifact carrying a small structured summary.
- FR2 — "Why did this PR merge?" is answerable from dossier alone, **given the owning project + the PR ref**: find the merge receipt, follow it to the verdict, read outcome / head sha / grant id — zero lookups in gate's or ship's stores for the *summary*, one `ref` hop for the *full* record. (The project scope is not a real cost: a PR URL already names `<owner>/<repo>`, which maps to the corpus project; `artifact.list` keeps `project` required to match the store seam — §6.)
- FR3 — Existing corpora, unknown kinds, and meta-less artifacts keep working unchanged (round-trip untouched, readers degrade gracefully).
- FR4 — `/wip`, `/shipped`, `flare`, and any LLM read one substrate via the existing `artifact.list` surface.

**Non-functional**

| Property | Target |
|---|---|
| Compatibility | old `artifacts.jsonl` rows (no `meta`) parse forever; new rows parse under old readers that ignore unknown fields |
| Density | one JSONL line per record; `meta` capped (§5) so git diffs stay dense and the file stays greppable |
| Plane purity | zero verdict-evaluation logic in dossier — structural validation only (caps, shape) |
| Durability | append-only `artifacts.jsonl` unchanged: `O_APPEND` + file lock, never rewritten |
| Query cost | FR2's read is ≤2 `artifact.list` calls on the existing surface |

## 3. Architecture overview

```
  Execution (ship driver)   Verification (gate, review-coordinator)   Capability (grants)
        │ run records              │ verdicts + audit refs                │ grant ids
        └──────────────┬───────────┴──────────────────┬──────────────────┘
                       ▼   artifact.link { kind, ref, label, meta }      (callers: skills /
            ┌──────────────────────────┐                                  close-out convention
            │  dossier  (State plane)  │  artifacts.jsonl — append-only   today; driver auto-
            └──────────┬───────────────┘  pointer + denormalized summary  wiring later)
                       ▼   artifact.list
  Observability: /wip · /shipped · flare · "why did this PR merge?"  (read one substrate)
```

Nothing new structurally: the Artifact primitive, `artifacts.jsonl`, and the two artifact verbs already exist. What changes: (a) `Artifact` gains an optional `meta` map, (b) `verdict` and `receipt` join the well-known kinds with documented meta-key conventions, (c) `artifact.list` grows a `ref` filter so a PR URL / run id resolves to its records. The emitting side stays **by convention** (skills call `artifact.link` at close-out) until the driver auto-wiring initiative lands.

## 4. Key decisions & trade-offs

| # | Decision | Alternative | Why |
|---|---|---|---|
| **D1** | dossier stores a **pointer + denormalized summary**; the authoritative record (gate's hash-chained decision, ship's full run) stays in its home store, reachable via `ref` | Full canonical copy in dossier | Copying re-homes cryptographic authority State can't honor (dossier doesn't verify chains — non-goal), invites drift, and bloats the jsonl. "Canonical" here means *the canonical place to ask*, not the canonical bytes. **Reviewers: this is the load-bearing call.** |
| **D2** | Extend the **Artifact** primitive (flat `meta` map) | New `Verdict` / `Receipt` primitives with own files + verbs + lifecycle | PROTOCOL.md already routes decisions through artifacts ("Not in v0"); kind is already extensible by design. New primitives = new layout, new verbs, new state for records that have no lifecycle — they're immutable facts, exactly what an append-only artifact is. |
| **D3** | **No new verbs.** `artifact.link` gains optional `meta`; `artifact.list` gains a `ref` filter and returns `meta` | `verdict.record` / `receipt.record` wrappers | Wrappers add surface without capability — the caller still supplies kind + fields. Smallest sharp API; a wrapper can be layered later if call sites prove error-prone. |
| **D4** | Structural validation only: meta caps, string shape. dossier never checks `outcome` values or re-derives verdicts | Validate outcome enums per kind | Outcome vocabularies belong to the emitters (gate's pass/blocked/parked/refused, driver judgments). Baking them into State couples planes and forces lockstep releases. Unknown keys/values round-trip untouched, like unknown kinds. |
| **D5** | `meta` is a **flat `string → string` map** with hard caps: ≤16 keys, key ≤64 bytes, value ≤512 bytes, ≤4 KiB total serialized | Nested JSON / free-form blob | Caps are the anti-drift mechanism: too small for prose or run logs (generic-doc-store drift structurally impossible), big enough for outcome + sha + ids. Flat map keeps the jsonl human-greppable. |
| **D6** | **Driver auto-wiring is out of scope** — a separate initiative, named as adjacent | Bundle it here | Genuinely separable: the substrate is immediately consumable by hand and by skill convention (close-out steps already exist in the loop); auto-wiring changes *ship and the skills*, not dossier's schema, and has its own failure modes (partial writes mid-run, retries). Sequencing them independently means the schema is proven by dogfood before an automated writer depends on it. |
| **D7** | Well-known kinds added: **`verdict`**, **`receipt`**. `grant` is **not** promoted — grant ids travel in `meta.grant` on verdicts | Also promote `grant` | Grants are Capability-plane objects minted and stored by gate; what State needs to remember is *which grant authorized this verdict*, which is one meta key. Promote `grant` when a grant-centric query ("what merged under grt_X?") shows up — it's answerable today via meta scan, just not indexed. |
| **D8** | Anchoring stays **project + optional task** (existing Artifact shape); phase is derived via the task | Add a `phase` field to Artifact | Nearly every verdict/receipt belongs to a task (the PR implements a task); phase-level anchoring adds a field + invariants for a case dogfood hasn't produced. Revisit on evidence (§10 Q4). |

## 5. Data model

`Artifact` (domain + jsonl) gains one optional field; everything else is unchanged:

| field | type | notes |
|---|---|---|
| `meta` | map string → string, optional | flat; caps per D5; omitted when empty (old rows unchanged) |

`kind` well-known set becomes `commit | pr | file | url | run | doc | verdict | receipt` — still extensible, unknown kinds still round-trip.

**Canonical `ref` form per kind** (the `ref` filter is exact-match — §6 — so the form must be pinned, not spelled three ways). PROTOCOL.md fixes one canonical `ref` per well-known kind:

- `kind: receipt` — `ref` = the canonical GitHub PR URL `https://github.com/<owner>/<repo>/pull/<n>` (no trailing slash, no `.git`, lowercase host). This is the form `artifact.list { ref }` matches.
- `kind: verdict` — `ref` = the gate audit ref (gate's opaque decision identifier, e.g. `gate://<repo>/pr/<n>/<dec_id>`), stable per decision.
- Readers that don't want to depend on exact `ref` formatting can instead join on the task anchor + `kind` + `meta.pr` (§7.2) — `ref` exact-match is the fast path, the task+`meta.pr` join is the format-independent fallback.

**Documented meta-key conventions** (conventions, not schema — unknown keys pass through; PROTOCOL.md carries this table). `actor` records *who linked the row* (the close-out caller); *who decided* is `meta.source` — the two are kept distinct so a skill-driven close-out and the gate that produced the verdict are both attributable:

- `kind: verdict` — `source` (`gate` | `review-coordinator` | …), `outcome` (emitter's vocabulary, e.g. gate's `pass`/`blocked`/`parked`/`refused`), `pr`, `head_sha`, `grant` (grt_ id, when one applied), `tier`.
- `kind: receipt` — `event` (`merge` | `close-out` | …), `pr`, `merge_sha`, `verdict` (the art_ id of the authorizing verdict — the FR2 fast-path join, with a fallback in §7.2), `supersedes` (art_ id, when this row corrects an earlier immutable one — §7.4).
- `kind: run` (existing, enriched by convention) — `engine`, `run` (ship run id), `judgment`.

**Example rows** (append-only `artifacts.jsonl`, one line each):

```jsonl
{"id":"art_01K…","project":"prj_01KRSZ…","task":"tsk_01K…","kind":"verdict","ref":"gate://dossier/pr/93/dec_01K…","label":"gate pass PR #93","linked_at":"2026-07-23T18:00:00Z","actor":"claude-code:michael","meta":{"source":"gate","outcome":"pass","pr":"93","head_sha":"872b472","grant":"grt_01K…","tier":"2"}}
{"id":"art_01K…","project":"prj_01KRSZ…","task":"tsk_01K…","kind":"receipt","ref":"https://github.com/itsHabib/dossier/pull/93","label":"merged PR #93","linked_at":"2026-07-23T18:05:00Z","actor":"claude-code:michael","meta":{"event":"merge","pr":"93","merge_sha":"a1b2c3d","verdict":"art_01K…"}}
```

`ref` points at the authoritative record (gate audit entry, ship run, PR URL); `meta` is the denormalized summary that makes one-substrate reads possible (D1). On-disk layout, ULIDs, and the append discipline in LAYOUT.md are untouched.

## 6. API contract

Two verb-surface changes, no new verbs (D3):

```
artifact.link  { project, task?, kind, ref, label, meta?, actor } → Artifact
  meta: optional flat map<string,string>; rejected with invalid_params when any D5 cap
  is exceeded (too many keys / key too long / value too long / total too large).
  No per-kind validation of keys or values (D4).

  DEDUP + IMMUTABILITY (changes the shipped dedup): link_artifact today dedups on
  (task, kind, ref) and returns the existing row. With meta, that dedup gains a
  meta comparison:
    - existing row found AND new meta byte-identical → return existing row (idempotent;
      re-running close-out after a crash is safe).
    - existing row found AND new meta differs → invalid_params
      ("meta is immutable for an existing (task, kind, ref); supersede instead").
    - no existing row → append.
  Verdicts/receipts are immutable facts; a wrong meta is corrected by SUPERSEDING
  (§7.4), never by mutation — consistent with append-only artifacts.jsonl.

artifact.list  { project, task?, kind?, ref? } → [Artifact]
  project: REQUIRED (matches ArtifactListFilter at the store seam, src/store.rs). A PR
  URL already names <owner>/<repo> → the corpus project, so a ref-lookup supplies it.
  ref: optional EXACT-MATCH filter over the canonical ref form for the kind (§5) — the
  caller passes the canonical GitHub PR URL (receipt) or gate audit ref (verdict) verbatim.
  Returned artifacts include meta when present.
```

Rust surface (`src/domain.rs`): `Artifact.meta: BTreeMap<String, String>` with `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]` — `BTreeMap` for deterministic serialization (stable git diffs). `LinkArtifact` gains the same field; cap enforcement **and the meta-immutability dedup check** live in the policy layer above the store (`link_artifact_outcome` in `src/server.rs`), per the policy/mechanism split — the store stays a dumb append. Both `FsStore` and `S3Store` round-trip the field.

**Error model:** cap violations → `invalid_params` with the failing key named; everything else unchanged.

## 7. Key flows

### 7.1 Write — gate close-out (by convention, today's emitter)
```
1. gate authorizes PR #N (exit 0) → decision artifact in gate's audit chain
2. close-out step (skill / operator agent) calls:
   artifact.link { project, task, kind: "verdict", ref: <gate decision ref>,
                   label: "gate pass PR #N", meta: { source, outcome, pr, head_sha, grant, tier } }
3. merge lands → artifact.link { kind: "receipt", ref: <PR URL>,
                   meta: { event: "merge", pr, merge_sha, verdict: <art_ id from step 2> } }
4. both appends: O_APPEND + file lock, one line each (LAYOUT.md, unchanged)
```

### 7.2 Read — "why did this PR merge?"
```
1. artifact.list { project, kind: "receipt", ref: <canonical PR URL> }   → merge receipt
2. receipt → verdict, two joins (the second is the fallback for an unenforced FK):
   FAST PATH:  receipt.meta.verdict (an art_ id) names the verdict; resolve it by
               listing verdicts for the same anchor and matching the id —
               artifact.list has no by-id fetch (a future artifact.get would
               collapse this to one hop).
   FALLBACK:   when meta.verdict is missing or dangles (it is a hand-maintained
               foreign key with no referential integrity), join on the shared task
               anchor: artifact.list { project, task: <receipt.task>, kind: "verdict" },
               disambiguated by meta.pr — the format-independent path.
3. verdict.meta: outcome, head_sha, grant, tier — the summary answer
4. full record: follow verdict.ref into gate's audit chain (one hop, by design — D1)
```
The `meta.verdict` FK is an *optimization*, not load-bearing: it saves a list call when
present and correct, but the task+`meta.pr` join always answers, so a wrong or missing FK
(including one written before the immutability rule, §7.4) degrades to slower, not broken.

### 7.3 Read — `/shipped` / `/wip` / flare
One `artifact.list { project, kind: "receipt" }` (or `verdict`) per project instead of joining ship's ledger + gate's audit + GitHub. Task anchoring gives the join to phases via the task's `phase` field.

### 7.4 Degraded / wrong-shape input, dedup, and correcting a bad meta
The shipped `link_artifact_outcome` **already dedups** on `(task, kind, ref)` — a second link with the same triple returns the existing row, it does not append. This TDD extends that dedup to reason about `meta` (§6), which makes verdicts/receipts *immutable facts*:

- **Idempotent retry** (crash between close-out steps, two close-out passes with the same data) → the dedup returns the existing row; safe, no duplicate. This is the common case and it Just Works.
- **Correcting a wrong meta** (e.g. a `meta.verdict` pointing at the wrong verdict art_ id): the same `(task, kind, ref)` with *different* `meta` is **rejected** with `invalid_params` ("meta is immutable; supersede instead"). You cannot silently overwrite — and because `artifacts.jsonl` is append-only with no update path, an immutable fact must be corrected by **superseding**, not mutating:
  - **Supersede convention:** append a *new* artifact with a **distinct `ref`** (the correction carries a fresh gate audit ref, or the PR URL with a `#v2` fragment for a re-recorded receipt) and `meta.supersedes: <art_id of the bad row>`.
  - **Reader rule:** among artifacts of a given `(kind, logical target)`, ignore any row that is named by a later row's `meta.supersedes`; take the survivor. This gives a deterministic "current" fact without an update path and without a fragile "latest `linked_at`" heuristic.
- **Missing / unknown meta** → link succeeds; readers show what's there (FR3). A missing `meta.verdict` on a receipt falls back to the task+`meta.pr` join (§7.2), so it degrades, not breaks.
- **Run log stuffed into `meta`** → cap rejection at link time (D5). The correct move is a `ref` to the log's home store.

(There is no `request_id` field on `LinkArtifact`; idempotency here comes from the `(task, kind, ref)` dedup above, not from a request token.)

## 8. Concurrency / consistency / failure model

Unchanged, deliberately: single-writer-per-corpus, append-only `artifacts.jsonl` with `O_APPEND` + file lock, atomic temp-file+rename for markdown. Verdicts/receipts are immutable facts — no update path, no CAS need. Two failure cases, two answers:

- **Crash between 7.1 step 2 and step 3** (verdict written, receipt not) → consistent: the merge simply hasn't been recorded yet. Repaired by re-running close-out — the `(task, kind, ref)` dedup (§7.4) makes the re-run idempotent for the already-written verdict and appends the missing receipt.
- **A fact was written with wrong `meta`** → *not* repaired by re-running close-out (the dedup rejects a differing-meta re-link, §7.4). The only correction is the **supersede** path (§7.4): append a new artifact with a distinct `ref` and `meta.supersedes`, and readers take the non-superseded survivor. This keeps "immutable append-only" honest — nothing is ever rewritten in place — while still giving operators a way to correct a mistake.

## 9. Rollout / implementation plan

Validation gate after Phase B — the substrate must prove itself on dossier's own PRs before any automated writer (driver auto-wiring) is designed against it.

| Phase | Goal | High-level tasks | Depends on | Gate | ~wLOC |
|---|---|---|---|---|---|
| **A — schema + verbs** | The substrate exists | 1. `meta` field end-to-end: domain, FsStore + S3Store round-trip, `artifact.link` arg, cap enforcement + **meta-immutability dedup** (identical→idempotent, differ→`invalid_params`; §6/§7.4) in the policy layer, PROTOCOL/LAYOUT updates in the same PR. 2. `artifact.list`: `ref` exact filter + `meta` in output, both stores. 3. Well-known kinds `verdict`/`receipt` + canonical-`ref` + meta-key + supersede convention tables (docs). | — | pre-gate | ≤300 + ≤200 + 0 (docs) |
| **B — dogfood** | Prove the reads | Record verdict + receipt artifacts for dossier's own merging PRs (starting with this TDD's PR) via close-out convention; backfill the last handful of merges; exercise 7.2/7.3 for real. | A | **GO/NO-GO** | ~0 (corpus writes) |
| **C — observability cutover** *(post-gate, mostly outside this repo)* | One substrate for reads | Point `/shipped`, `/wip`, flare sweeps at `artifact.list`; retire their multi-store joins. | B | post-gate | — |
| **D — driver auto-wiring** *(adjacent initiative, own TDD)* | Writes as a side effect of working | ship/driver + skills emit 7.1 automatically at land/record; not designed here (D6). | B | separate TDD | — |

## 10. Open questions

1. **Promote `grant` to a well-known kind?** (D7) Held at a meta key. If "what merged under grant X" becomes a real recurring query, promote it.
2. **Gate as the required status check** (option (c) for the solo-repo `REVIEW_REQUIRED` deadlock): if gate's verdict becomes the branch-protection approval of record, does the verdict artifact's `ref` point at the GitHub check run or gate's audit entry? Leaning audit entry (the check is a projection), but this is gate's call, not dossier's — flagging so the schemas don't diverge.
3. **Backfill depth** — Phase B backfills "a handful" of recent merges by hand. Deeper historical backfill is only worth tooling if a read needs it; default no.
4. **Phase-level anchoring** (D8) — watch dogfood for a verdict/receipt that genuinely has no task. Two occurrences = revisit.
5. **`search` over meta** — meta is excluded from the `search` index for now (it's structured, not prose). If "find the parked verdicts" wants free-text search rather than `artifact.list` filters, revisit.

## 11. Validation plan (the gate)

After Phase A ships and Phase B runs for the next **5 merged dossier PRs**: for each, the question *"which verdict authorized this merge, with what outcome, under which grant?"* is answered by `artifact.list` calls alone — **5/5, zero reads of gate's store or ship's ledger for the summary** (the `ref` hop to the full record is allowed and expected). Binary, baseline-free.

Two conditions harden the gate against author bias — proving the schema is **self-describing**, not just that its author can query rows he wrote:

1. All 5 close-outs are written by the **normal close-out flow following only the PROTOCOL.md convention tables** (canonical `ref`, meta keys, supersede) — *not* by consulting this TDD. If the convention tables are insufficient to emit a correct row, that is a gate failure, fixed in the docs before proceeding.
2. At least **1 of the 5 reads is performed by a fresh session given only PROTOCOL.md** (no this-TDD context). If it can't reconstruct verdict→outcome→grant from the protocol alone, the schema isn't self-describing yet.

Green → Phase C cutover and the driver auto-wiring TDD get written against a proven schema. Red (missing joins, meta too thin/too fat, convention tables underspecified) → fix the conventions before any automation depends on them.

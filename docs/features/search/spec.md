**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-16
**Related**: [horizon.md](../../horizon.md), [filter-expansion/spec.md](../filter-expansion/spec.md), [vision.md](../../vision.md), [PROTOCOL.md](../../../PROTOCOL.md)

# Search — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/domain.rs`, `src/store.rs`, `src/server.rs` | ~250 | 250 |
| Tests | `src/store.rs` test module, `src/server.rs` test module | ~200 | 100 |
| **Total** | | | **~350** |

Band: **amazing** (<500 weighted). Single PR.

## Goal

A single MCP verb that performs literal substring search across project, phase, and task **titles + bodies** in one call, returning ranked snippets. The "have I designed / done this before?" question — natively cross-primitive.

Why a dedicated verb instead of `body_contains` across the three list verbs (which filter-expansion already provides):
- Without `search`, the LLM needs three round-trips (project.list + phase.list + task.list), each with the same `body_contains`, then merge + rank client-side.
- `search` returns a unified, ranked result set in one call so the LLM goes: one call → one ranked list → pick what to read.
- Cross-primitive ranking matters: a project body match and a task body match should compete on the same scale, not be sorted within their own buckets.

## Behavior

```
search {
  query: string,                              # literal substring; required, non-empty
  kinds?: ("project" | "phase" | "task")[],   # default: all three
  project?: string | null,                    # null = corpus-wide
  limit?: number,                             # default: 50
}
→ [{
  kind: "project" | "phase" | "task",
  id: string,
  project: string,
  phase?: string,                             # set when kind=task
  slug: string,
  title: string,
  snippet: string,                            # ~80 chars around first match
  score: number,                              # v1: literal match count; future: hybrid lexical+semantic
}]
```

## Decisions

- **Case-insensitive literal substring** — same primitive as filter-expansion's `body_contains`. Lowercase both sides; no tokenization, no regex.
- **Search target: title + body** for every primitive. Titles are short and direct; missing a title match would be a bug for queries like "auth migration."
- **Other frontmatter not searched** — slug / status / assignee aren't free text. Filter-expansion handles structured queries on those.
- **Snippet: ~80 chars** centered on the first match, no markdown awareness. Simple, fast, plenty for an LLM to decide whether to read the full body.
- **Ranking**: `(score desc, updated_at desc)`. Both signals matter; recency tiebreaker keeps "what I was just thinking about" surfaced. Score is named generically so future vector / hybrid search doesn't break the response shape (see Forward compatibility).
- **Empty query is a validation error** — not a "return everything" command. Use the list verbs for that.
- **`limit` applies after ranking**. Default 50.
- **No regex, no fuzzy / semantic / vector search.** Strictly literal. Vectors are the upgrade path if evidence demands.
- **Cross-primitive in one call.** The LLM doesn't compose; the server walks.

## Non-goals

- No persistent index. Each call walks the corpus. At v0 sizes (thousands of files, not millions) the naive walk is fine.
- No tokenization, stemming, lemmatization.
- No relevance-feedback learning.
- No Boolean queries (`auth AND oauth NOT legacy`). One literal string per call.
- No match-offset metadata. Snippet is enough; the LLM doesn't need offsets.
- No removal of `body_contains` from filter-expansion. The single-primitive filter is still useful when you know what kind of thing you're looking for.
- No search over `artifacts.jsonl` labels. Artifacts are dense pointers, not content; add `artifact_kind?` later if evidence shows it's missed.

## Acceptance

- `search { query: "auth" }` returns project / phase / task rows where "auth" appears in title or body, ranked.
- `search { query: "Cheney" }` finds the "Cheney-style clippy" phase in the dossier dogfood corpus.
- `search { query: "follow-ups" }` returns multiple hits across phase + tasks.
- `search { query: "nonexistent-zzzz" }` returns `[]`, not an error.
- `search { query: "" }` returns a typed validation error.
- `search { query: "auth", kinds: ["task"] }` returns only task hits.
- `search { query: "auth", project: "wellness-ai" }` restricts to one project.

## Test plan

- **Per-primitive hit tests**: substring in project body, phase body, task body, plus title hits for each.
- **Cross-primitive query** that hits all three kinds, asserting ranking is uniform across kinds.
- **Ranking test**: row with 3 matches ranks above row with 1; ties broken by `updated_at desc`.
- **`kinds` filter**: only the requested kinds returned.
- **`project` filter**: cross-project vs single-project.
- **Validation**: empty `query` → typed error; bad `kinds` value → typed error.
- **No-match**: empty array, not an error.
- **`limit`**: top-N respected after ranking.
- **Dogfood corpus**: searches over `projects/dossier/` for the acceptance queries above; asserts expected rows.

## Implementation sketch

- `domain.rs`: `SearchHit`, `SearchArgs`, `SearchKind` types. `SearchKind` is `Project | Phase | Task`.
- `store.rs`: `FsStore::search()` walks the corpus (reuses the existing per-primitive readers), counts case-insensitive substring matches in `title.to_lowercase() + "\n" + body.to_lowercase()`, builds an 80-char snippet around the first match, sorts by `(score desc, updated_at desc)`, applies limit. Score in v1 is the literal match count cast to whatever numeric type the response uses.
- `server.rs`: `search` tool exposing the args; doc string surfaces every decision (case-insensitive, title+body, snippet length, ranking) so the LLM understands when to call `search` vs `body_contains` on a list verb.
- No new file format, no new on-disk artifacts, no cache layer.

## Forward compatibility

Vector / hybrid search is on the horizon (horizon.md Phase ∞) but explicitly out of scope for v1. The v1 surface is shaped so that addition is a self-contained chunk, not a rewrite — the public response and query shapes are stable, and the implementation can be swapped underneath.

What's stable by design:

- **`score: number`** in the response (not `match_count`). v1 = literal match count cast to a number. Future hybrid = `(lexical_weight × literal_count) + (semantic_weight × cosine_similarity)`. Caller's contract — *"sort by score desc, updated_at desc, take top limit"* — never changes when the implementation grows a vector path.
- **Single `query: string`, no `mode` flag.** Future vector search is auto-blended into the same call; the caller doesn't pick between modes. If evidence ever demands explicit control, a `mode?: "literal" | "hybrid"` field can be added later without breaking existing callers (additive, default = current behavior).
- **Snippet has a graceful fallback.** v1: ~80 chars around the first literal match. Future hybrid: if a row matched only semantically (no literal match position), snippet falls back to first ~80 chars of body. Degraded but not broken; same field name, same type.
- **Index is implementation-internal.** v1 has no on-disk index. Future vectors add `.dossier/cache/embeddings.sqlite` (gitignored per LAYOUT.md, regenerated on write). Callers never see the cache; deleting it degrades search to literal-only until the next index rebuild.

What this does NOT preserve a path for:

- Multi-term Boolean queries (`auth AND oauth NOT legacy`) — different shape, different verb if ever needed.
- Multiple query strings in one call. One string per call.
- Returning the score's component breakdown (lexical vs semantic). If a caller wants to know *why* a row ranked highly, that's a different request.

Cost of this forward-compat in v1:

- `score` is a slightly less obvious name than `match_count` for v1 callers. Mitigated by the tool description: *"score is the relevance signal — higher is more relevant; v1 is literal match count."*
- Zero LOC cost. The field is named differently; everything else is unchanged.

### Backend strategy (when additional backends land)

Vector / hybrid search ships as an additional backend behind a Cargo feature flag, not as a rewrite of `FsStore::search()`. Anticipated shape:

```toml
[features]
default = ["search-literal"]
search-literal = []                       # v1 backend; always present
search-sqlite-vec = ["dep:sqlite-vec"]    # opt-in SQL extension dep
search-inproc = []                        # bespoke: flat embeddings file + in-RAM cosine
```

A `SearchBackend` trait gets extracted **when the second backend is added** — not now. Defining the trait against a single implementation bakes in the wrong abstraction (we don't yet know whether vector backend #1 will be sqlite-vec, lancedb, a bespoke in-RAM impl, or something else). YAGNI — keep v1 concrete; refactor under the pressure of the real second impl.

When the trait lands:

- `LiteralBackend` (the v1 code, refactored into a single impl) is always available; default.
- Additional backends ship behind their own Cargo feature; the dep cost is opt-in.
- Runtime selection via `.dossier/config.toml` (e.g., `search.backend = "literal" | "sqlite-vec" | "inproc"`), defaulting to whatever's compiled in.

**`search-inproc` vs `search-sqlite-vec`** is a real future decision, not a pre-commitment. At solo-dev sizes the math favors in-process: ~hundreds of projects × thousands of tasks ≈ 50k rows max; 1536-dim float32 embeddings × 50k × 4 bytes = ~300MB, fits in RAM; cosine across the lot is microseconds. No SQL extension, no schema, no migration. sqlite-vec earns its complexity when row counts pass ~1M. Pick based on evidence at the time, not now.

**Behavioral variance across builds is acceptable** because dossier is a solo-dev tool you compile for yourself. A user with `--features search-sqlite-vec` gets hybrid score derivation; one without gets literal-only. Same caller contract (score-ordered ranked hits with snippets); different score arithmetic underneath. Document the compiled feature set in `mcp_version` or a similar startup banner so it's not invisible.

## Open questions

- **Snippet boundary** — chop at exactly 80 chars or respect word boundaries? Lean chop-at-80 for simplicity; revisit if snippets read poorly.
- **Title-match weight in ranking** — should a title match count more than a body match? Lean no for v1 (simpler); revisit if title hits get buried.
- **Performance at scale** — naive walk is fine at v0. If the portfolio grows to 100+ projects with deep history, the upgrade path is a SQLite full-text sidecar regenerated on write. Same evidence-driven trigger as vectors.
- **Project-description searches** — `Project.description` is the body for projects. Confirm in implementation that this is the field walked (not a separate "summary" field that doesn't exist).

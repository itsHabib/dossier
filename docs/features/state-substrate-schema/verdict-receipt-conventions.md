**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-07-23
**Related**: dossier task `verdict-receipt-conventions` (id: `tsk_01KY86EVKAK6ZC682MZ3NERY5M`), [state-substrate TDD](../state-substrate/spec.md) §5/§7.4. **Depends on** `artifact-meta-field` (documents the shipped shape).

# Document well-known kinds verdict/receipt + meta-key conventions — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `PROTOCOL.md`, `LAYOUT.md`, `CLAUDE.md` | ~90 | 0 (docs) |
| **Total** | | | **~0 weighted** |

Docs-only PR (0× weight). No production source, no tests beyond `make check` staying green.

## Goal

Emitters (gate close-out, driver close-out, skills) need a shared vocabulary for verdict/receipt `meta` keys, or the substrate fragments into per-caller dialects. Transcribe the locked convention tables from the TDD into `PROTOCOL.md` + `LAYOUT.md` so the schema is **self-describing from the protocol alone** — the TDD §11 validation gate requires a fresh session to emit and read correct rows given only `PROTOCOL.md`.

## Behavior / fix

Docs-only. From the locked TDD §5:

- Add `verdict` and `receipt` to the well-known kind list in `PROTOCOL.md` + `LAYOUT.md`: `commit | pr | file | url | run | doc | verdict | receipt` — still extensible, unknown kinds still round-trip.
- Carry the **canonical `ref` form per kind** (TDD §5): `receipt` → canonical GitHub PR URL `https://github.com/<owner>/<repo>/pull/<n>` (no trailing slash, no `.git`, lowercase host); `verdict` → gate audit ref (gate's opaque per-evaluation id, a `run_…` id today, e.g. `gate://<repo>/pr/<n>/<gate_run_id>`).
- Carry the **meta-key convention tables** (TDD §5) with one example jsonl row each:
  - `verdict`: `source`, `outcome`, `pr`, `head_sha`, `grant`, `tier`.
  - `receipt`: `event`, `pr`, `merge_sha`, `verdict` (art_ id), `supersedes` (art_ id, when correcting an earlier immutable row).
  - `run` (existing, enriched): `engine`, `run`, `judgment`.
- Carry the **supersede convention** (TDD §7.4): correct an immutable row by appending a new artifact with a distinct `ref` + `meta.supersedes: <art_id>`; readers ignore any row named by a later row's `meta.supersedes`.
- State explicitly: **conventions, not schema** — unknown keys round-trip; dossier never validates `outcome` vocabularies (TDD §4 D4).
- Update the `CLAUDE.md` **State** section to note the substrate (meta + verdict/receipt kinds + ref filter) is live.

## Acceptance

- `PROTOCOL.md` + `LAYOUT.md` name both kinds and the convention tables; example rows parse against the shipped `meta` schema (from `artifact-meta-field`).
- `CLAUDE.md` State section mentions the substrate is live.

## Test plan

- Docs-only; `make check` green (no code).
- Example rows lint-checked by pasting into the round-trip test corpus if cheap (optional).

## Non-goals

- Any emitter implementation (driver auto-wiring is a separate TDD, D6).
- Per-kind validation logic (rejected by design, D4).
- The `meta` field / `ref` filter code — landed by the sibling tasks.

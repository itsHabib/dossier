**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-01
**Related**: dossier task `cli-artifact-double-read` (id: `tsk_01KT1WQ3M3V9TMCN101YF7RQ8G`), [docs/follow-ups.md](../../follow-ups.md)

# Dedup the CLI artifact-link double-read — design spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/bin/dossier.rs` (`run_artifact_link`) | ~15 | 15 |
| **Total** | | | **~15** |

Band: **amazing** (one-screen). Runtime: **local**.

## Goal

`run_artifact_link` (`src/bin/dossier.rs`) pre-checks `artifacts.jsonl` for an existing link via `FsStore`, then calls `svc.link_artifact`, which does the same idempotency scan — reading the file twice on the idempotent path.

## Behavior / fix

Lean on the service's idempotent return exclusively: drop the CLI pre-check and let `svc.link_artifact` handle the dedup (it already returns the existing artifact idempotently on a matching `(task, kind, ref)`). If the CLI's "already linked" diagnostic (`eprintln`) is worth keeping, surface it from the service's idempotent-return path rather than re-scanning the file in the CLI.

## Acceptance

- The idempotent artifact-link path reads `artifacts.jsonl` once.
- Behavior is unchanged for both the new-link and already-linked cases (same exit code + any diagnostic).

## Test plan

- `make check` green.
- Existing artifact-link CLI behavior preserved: a re-link of the same `(task, kind, ref)` is still a no-op with the same output.

## Non-goals

- Changing the service-layer idempotency semantics.

## Source

PR #69 review (claude finding 4). Minor.

**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-31
**Related**: dossier task `lift-phase-project-artifact-drop-fs` (id: `tsk_01KSZT09V65EJJYGMBZC2NJH9B`), phase `policy-mechanism-separation`, [docs/follow-ups.md](../../follow-ups.md)

# Lift phase/project/artifact verbs + drop the fs field — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (5 verbs → CAS, drop `fs` field), `src/store.rs` (delete inherent write methods) | ~270 | 270 |
| Tests | `src/store.rs` `add_phase` CAS race tests retargeted through the service path | ~60 | 30 |
| **Total** | | | **~300** |

Band: **amazing**. Single PR. **Depends on `lift-task-state-machine`** (must land after — this drops the `fs` field the task verbs rely on until they're lifted).

## Goal

After the task verbs lift (`lift-task-state-machine`), the phase (add / update), project (create / update), and artifact (link) verbs still run on `FsStore` via `self.fs`, and `MeshService` still carries the concrete `fs: Arc<FsStore>` field — the last of the policy/mechanism violation. The hardest remaining piece is `add_phase`'s order logic (the CAS-gated `project.md` + file shifting).

## Behavior / fix

Lift `add_phase` / `update_phase` / `create_project` / `update_project` / `link_artifact` into `MeshService` over `self.store`. `add_phase` becomes:

```
get phases + project (versioned) → domain order-compute → CAS-gate project.md (put_project expected = version) → put_phase + shift
```

mirroring the existing `try_add_phase_once` pattern but at the service layer. Delete the remaining inherent `FsStore` write methods. Then **drop the `fs: Arc<FsStore>` field** from `MeshService` (`src/server.rs:44`) — the service now runs on `Arc<dyn Store>` alone. `FsStore` is left implementing only the `Store` trait + `open` / `root`.

## Acceptance

- `MeshService` holds only `store: Arc<dyn Store>` (+ `write_lock`) — no concrete `FsStore` handle in `server.rs`.
- All 9 write verbs run over the trait.
- The `add_phase` CAS race tests (`src/store.rs:5510+`) pass through the service path.
- `FsStore` has no inherent write methods.

## Test plan

- `make check` green.
- Existing phase / project / artifact verb tests + `add_phase` CAS race tests pass via the service.
- `grep -n "Arc<FsStore>\|self.fs" src/server.rs` returns nothing.

## Non-goals

- Wiring S3 into bin + config so the server boots on S3 (`DOSSIER_BACKEND=s3`, construct `S3Store`) — the next-unblock named in the phase body; materialize as its own task when this lands.
- Warm cache / read latency (a separate step after).

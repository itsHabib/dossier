**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-31
**Related**: dossier task `lift-phase-project-artifact-drop-fs` (id: `tsk_01KSZT09V65EJJYGMBZC2NJH9B`), phase `policy-mechanism-separation`, [docs/follow-ups.md](../../follow-ups.md)

# Lift phase/project/artifact verbs + drop the fs field — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (5 verbs → CAS, drop `fs` field), `src/store.rs` (delete inherent write methods; `shift_phases` trait primitive + impl per option A) | ~300 | 300 |
| Tests | `src/store.rs` `add_phase` CAS race + shift-orphan tests through the service path | ~70 | 35 |
| **Total** | | | **~335** |

Band: **amazing**. Single PR. **Depends on `lift-task-state-machine`** (must land after — this drops the `fs` field the task verbs rely on until they're lifted).

## Goal

After the task verbs lift (`lift-task-state-machine`), the phase (add / update), project (create / update), and artifact (link) verbs still run on `FsStore` via `self.fs`, and `MeshService` still carries the concrete `fs: Arc<FsStore>` field — the last of the policy/mechanism violation. The hardest remaining piece is `add_phase`'s order logic (the CAS-gated `project.md` + file shifting).

## Behavior / fix

Lift `add_phase` / `update_phase` / `create_project` / `update_project` / `link_artifact` into `MeshService` over `self.store`. `add_phase` becomes:

```
get phases + project (versioned) → domain order-compute → CAS-gate project.md (put_project expected = version) → put_phase + shift
```

mirroring the existing `try_add_phase_once` pattern but at the service layer.

#### The `shift` step needs a trait primitive — `Arc<dyn Store>` has no atomic rename

`try_add_phase_once` shifts displaced phases by renaming files: `fs::rename` of `{N}.<slug>.md` → `{N+1}.<slug>.md`, descending, then `write_atomic` the bumped content. The `Store` trait has no rename — `put_phase(expected = Some(v))` CAS-writes to the path `find_phase_path(&slug)` returns, the *old* filename, so the content gets the new order number but the file never moves, orphaning `{N}.<slug>.md` next to the new `{N+1}.<slug>.md`. The shift has to be expressed through the trait, and that shapes the `S3Store` surface — so it must be declared before the agent starts unit 3.

**Decision to confirm at implementation — recommended: option A.**
- **A (recommended).** Add a mechanism primitive to the `Store` trait: `shift_phases(project, from_order) -> Result<(), StoreError>` that bumps every phase at/above `from_order` by one. `FsStore` implements it with the existing rename loop; `S3Store` as copy+delete (or its batch-rename idiom). The **policy** (compute the new order, decide to shift) stays in the service; the trait gains only the **mechanism** (atomic reorder). This is the on-thesis choice for a policy/mechanism phase — the trait stays a thin mechanism, not a verb.
- **B.** Lift `add_phase` as a single composite trait method that owns the gate + shift internally. Simpler call site, but pushes policy *into* the trait — the exact inversion this phase exists to undo. Pick A unless a concrete S3 atomicity constraint forces B.

Either way both backends must shift identically; record the choice here before the agent starts so `FsStore` and `S3Store` don't diverge.

Delete the remaining inherent `FsStore` write methods. Then **drop the `fs: Arc<FsStore>` field** from `MeshService` — the service now runs on `Arc<dyn Store>` alone. `FsStore` is left implementing only the `Store` trait + `open` / `root` (plus the `shift_phases` mechanism under option A).

## Acceptance

- `MeshService` holds only `store: Arc<dyn Store>` (+ `write_lock`) — no concrete `FsStore` handle in `server.rs`.
- All 9 write verbs run over the trait.
- The `add_phase` CAS race tests pass through the service path.
- The phase `shift` runs through the trait (per the option-A/B decision); after an insert that displaces phases there are **no orphaned `{N}.<slug>.md` files** — every displaced file is renamed, not duplicated — and both backends shift identically.
- `FsStore` has no inherent write methods.

## Test plan

- `make check` green.
- Existing phase / project / artifact verb tests + `add_phase` CAS race tests pass via the service.
- An `add_phase` that displaces ≥1 existing phase leaves the phases dir with exactly the expected `{order}.<slug>.md` files and no orphans (proves the shift went through the trait primitive, not a stale-path write).
- `grep -n "Arc<FsStore>\|self.fs" src/server.rs` returns nothing.

## Non-goals

- Wiring S3 into bin + config so the server boots on S3 (`DOSSIER_BACKEND=s3`, construct `S3Store`) — the next-unblock named in the phase body; materialize as its own task when this lands.
- Warm cache / read latency (a separate step after).

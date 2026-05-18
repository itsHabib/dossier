---
driver_version: 1
generated_at: 2026-05-17T23:00:00Z
generated_by: work-driver-prep
source:
  project: dossier
  phase: hygiene-followups
repo: dossier
branch_prefix: tower/hygiene-

batches:
  - id: 1
    label: ready now
    depends_on: []
    status: done
    completed_at: 2026-05-18T02:25:30Z
    streams:
      - task_id: tsk_01KRSZFXMNZYKCMWYV3Z85E6XJ
        task_slug: actor-on-update-verbs
        spec_path: docs/features/hygiene-followups/actor-on-update-verbs.md
        branch_name: tower/hygiene-actor-on-update-verbs
        touches: [src/server.rs]
        status: done
        pr_number: 24
        merge_commit: e966e87ddf963f19b0e7e13e5424cd9162893532
        merged_at: 2026-05-18T02:22:26Z
        cycles: 2
      - task_id: tsk_01KRV8788KM9JCS9YDWD0H6WJ5
        task_slug: sort-determinism-secondary-key
        spec_path: docs/features/hygiene-followups/sort-determinism-secondary-key.md
        branch_name: tower/hygiene-sort-determinism
        touches: [src/store.rs]
        status: done
        pr_number: 25
        merge_commit: d6085a7f1c4f7603f174170e5a1ee81423fc4792
        merged_at: 2026-05-18T02:24:10Z
        cycles: 2
      - task_id: tsk_01KRW29YMNAG063612PHZY4EH6
        task_slug: phase-slug-cross-project-collision
        spec_path: docs/features/hygiene-followups/phase-slug-cross-project-collision.md
        branch_name: tower/hygiene-phase-slug-collision
        touches: [PROTOCOL.md]
        status: done
        pr_number: 26
        merge_commit: 4cdcf075cf3716d1e249a5de3d5af2da0bf2fa18
        merged_at: 2026-05-18T02:23:28Z
        cycles: 2

  - id: 2
    label: after batch 1
    depends_on: [1]
    status: done
    completed_at: 2026-05-18T03:13:45Z
    streams:
      - task_id: tsk_01KRSZG60JG3S0JF294AA3459V
        task_slug: uniform-error-data-taxonomy
        spec_path: docs/features/hygiene-followups/uniform-error-data-taxonomy.md
        branch_name: tower/hygiene-uniform-errors
        touches: [src/server.rs]
        status: done
        pr_number: 28
        merge_commit: da40087f83bfe9e8d009a63eb973c99de67c8bf7
        merged_at: 2026-05-18T03:13:32Z
        cycles: 2
      - task_id: tsk_01KRSZFQNBR0HNTV12D2D97MTH
        task_slug: slug-validation-remaining-paths
        spec_path: docs/features/hygiene-followups/slug-validation-remaining-paths.md
        branch_name: tower/hygiene-slug-validation
        touches: [src/store.rs]
        status: done
        pr_number: 29
        merge_commit: c17ba6f82e4ce755db47466ae46d7a05c6713050
        merged_at: 2026-05-18T03:13:45Z
        cycles: 2

  - id: 3
    label: after batch 2
    depends_on: [2]
    status: done
    completed_at: 2026-05-18T05:25:10Z
    streams:
      - task_id: tsk_01KRSZFZY8DGTE19QSRYV0W3BA
        task_slug: phase-created-by-parity
        spec_path: docs/features/hygiene-followups/phase-created-by-parity.md
        branch_name: tower/hygiene-phase-created-by
        touches: [src/domain.rs, src/store.rs, src/server.rs, LAYOUT.md]
        status: done
        pr_number: 31
        merge_commit: 1efbd9288192a672a3d39f3760fbfe98a82d74fa
        merged_at: 2026-05-18T05:25:10Z
        cycles: 2

  - id: 4
    label: after batch 3
    depends_on: [3]
    status: done
    completed_at: 2026-05-18T05:48:07Z
    streams:
      - task_id: tsk_01KRSZFVCFPYRN3Q7RCFGQ89HQ
        task_slug: clean-update-project-not-found
        spec_path: docs/features/hygiene-followups/clean-update-project-not-found.md
        branch_name: tower/hygiene-clean-update-project
        touches: [src/store.rs]
        status: done
        pr_number: 33
        merge_commit: d9fc35c37f3d26fb8f4386ea7d7ef0e796c750dc
        merged_at: 2026-05-18T05:48:07Z
        cycles: 1

  - id: 5
    label: after batch 4
    depends_on: [4]
    status: done
    completed_at: 2026-05-18T05:48:52Z
    streams:
      - task_id: tsk_01KRSZG2ZYV3BTY6E8HCPHG8S1
        task_slug: frontmatter-field-drift-test
        spec_path: docs/features/hygiene-followups/frontmatter-field-drift-test.md
        branch_name: tower/hygiene-frontmatter-drift
        touches: [src/store.rs]
        status: done
        pr_number: 34
        merge_commit: 36ed39d57c9c04ea080f288c1da94c4f78c28476
        merged_at: 2026-05-18T05:48:52Z
        cycles: 1

  - id: 6
    label: after batch 5
    depends_on: [5]
    status: done
    completed_at: 2026-05-18T06:04:10Z
    streams:
      - task_id: tsk_01KRW29PGBCV2MDQ7KJTMJ27EJ
        task_slug: task-get-by-id-verb
        spec_path: docs/features/hygiene-followups/task-get-by-id-verb.md
        branch_name: tower/hygiene-task-get-by-id
        touches: [src/store.rs, src/server.rs, src/domain.rs]
        status: done
        pr_number: 36
        merge_commit: baad7bdeb4284c1f52b6088b2a1adcbd12b123ac
        merged_at: 2026-05-18T06:04:10Z
        cycles: 1

conflict_notes:
  - kind: file_overlap
    file: src/store.rs
    tasks: [slug-validation-remaining-paths, clean-update-project-not-found, sort-determinism-secondary-key, frontmatter-field-drift-test, task-get-by-id-verb, phase-created-by-parity]
  - kind: file_overlap
    file: src/server.rs
    tasks: [actor-on-update-verbs, uniform-error-data-taxonomy, clean-update-project-not-found, task-get-by-id-verb, phase-created-by-parity]
  - kind: dep_signal
    from: phase-created-by-parity
    to: actor-on-update-verbs
    reason: "actor field semantics — actor stays on create verbs; phase-created-by uses it in add_phase. naturally respected because file conflict puts them in different batches anyway."

skipped_during_resolution:
  - reason: "ship project_get response exceeded MCP output limit (94k chars); could not enumerate its phases"
    workaround: "no impact on this run — hygiene-followups exists only in dossier. file follow-up: dossier needs a project.get summary variant (no bodies). horizon.md already gestures at this."
---

# Hygiene-followups execution plan

Generated by `/work-driver-prep phase:hygeine-followups` (operator typed `hygeine` — resolved to `hygiene-followups` in dossier via close-spelling match) on 2026-05-17. Consumed by `/work-driver docs/features/hygiene-followups/driver.md`.

## Source

Phase `hygiene-followups` in project `dossier` — 9 todo tasks at time of prep.

## Topology

Six batches, sequenced by file-overlap conflicts on `src/store.rs` and `src/server.rs` (the hotspots). Wall-clock estimate: ~4–6 hours depending on review cadence; most of it idle wait between batches.

## Batches

### Batch 1 — ready now (3 parallel-safe)

| Task | Touches |
|---|---|
| `actor-on-update-verbs` | `src/server.rs` (arg struct edits, narrow) |
| `sort-determinism-secondary-key` | `src/store.rs` (sort fns only, narrow) |
| `phase-slug-cross-project-collision` | `PROTOCOL.md` (docs only) |

### Batch 2 — after Batch 1 merges (2 parallel-safe)

| Task | Touches |
|---|---|
| `uniform-error-data-taxonomy` | `src/server.rs` (wide-blast: every handler) |
| `slug-validation-remaining-paths` | `src/store.rs` (wide-blast: every slug-derived path) |

### Batch 3 — after Batch 2 (1 stream)

| Task | Touches |
|---|---|
| `phase-created-by-parity` | `src/domain.rs` + `src/store.rs` + `src/server.rs` + `LAYOUT.md`. Dep on `actor-on-update-verbs` satisfied by Batch 1. |

### Batch 4 — after Batch 3 (1 stream)

| Task | Touches |
|---|---|
| `clean-update-project-not-found` | `src/store.rs` (probably `src/server.rs` indirectly) |

### Batch 5 — after Batch 4 (1 stream)

| Task | Touches |
|---|---|
| `frontmatter-field-drift-test` | `src/store.rs` test module |

### Batch 6 — after Batch 5 (1 stream)

| Task | Touches |
|---|---|
| `task-get-by-id-verb` | `src/store.rs` + `src/server.rs` + `src/domain.rs` |

## Conflict topology

The store.rs and server.rs hotspots dominate. The narrow / wide-blast distinction is heuristic — if the agent's actual edits prove narrower (e.g. `sort-determinism` truly only touches the sort functions), `/work-driver`'s Step 6 rebase pattern lets a subsequent merge proceed without redoing the whole batch.

## Dep signal

`phase-created-by-parity` depends on `actor-on-update-verbs` per the body of `phase-created-by-parity.md`. Both end up in different batches anyway due to file conflict, so the dep is naturally respected — but documented here so the dep doesn't surprise a future re-prep.

## Skipped during resolution

`mcp__dossier__project_get { slug: "ship" }` exceeded the MCP output limit (94k chars). No impact on this run — `hygiene-followups` is exclusive to dossier — but worth filing a follow-up: dossier needs a `project.get` summary variant (or pagination) so the corpus walk doesn't fail closed on projects with deep history.

---
id: phs_01J05M3K2N4F8Z7K3N9P5Q1R8W
project: prj_01J05M2WXG4F8Z7K3N9P5Q1R6T
slug: storage-layout
title: Define on-disk storage layout
order: 2
status: done
created_at: 2026-05-10T00:00:00Z
updated_at: 2026-05-10T00:00:00Z
---

Spec the on-disk markdown convention the mesh reads/writes. Two
constraints: git is the source of truth; humans can read and edit files
directly.

Acceptance:

- [x] LAYOUT.md exists at corpus root
- [x] Directory tree specified (corpus / projects / phases / tasks)
- [x] Frontmatter schemas given for project / phase / task
- [x] Append-only artifacts.jsonl format defined
- [x] Concurrency model documented (single-writer assumption)
- [x] Index/cache directory marked as gitignored
- [x] Migration story sketched (mesh_version field)

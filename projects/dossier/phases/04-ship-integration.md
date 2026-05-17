---
id: phs_01J05M3K4N4F8Z7K3N9P5Q1RAY
project: prj_01J05M2WXG4F8Z7K3N9P5Q1R6T
slug: ship-integration
title: Make ship a conforming APP client
order: 4
status: pending
created_at: 2026-05-10T00:00:00Z
updated_at: 2026-05-10T00:00:00Z
---

ship's kickoff/handoff outputs become structured writes through the dossier
mesh. ship stays in TS — it talks to the mesh as an MCP client. First
real-world consumer of the protocol. Consider **Cheney-style clippy** lints
for task bodies so ship never invents frontmatter keys.

Acceptance:

- [ ] ship's kickoff flow calls `project.create` + `phase.add` instead of
  writing files directly
- [ ] ship claims tasks via `task.claim` and posts progress via
  `task.update` with notes
- [ ] ship links produced PRs/commits via `artifact.link`
- [ ] One end-to-end project driven from ship kickoff through
  task-by-task implementation, with the resulting corpus diffable in git

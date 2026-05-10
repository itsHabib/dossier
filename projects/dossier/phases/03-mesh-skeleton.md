---
id: phs_01J05M3K3N4F8Z7K3N9P5Q1R9X
project: prj_01J05M2WXG4F8Z7K3N9P5Q1R6T
slug: mesh-skeleton
title: Bootstrap Go mesh skeleton
order: 3
status: pending
created_at: 2026-05-10T00:00:00Z
updated_at: 2026-05-10T00:00:00Z
---

Stand up the Go mesh: MCP server exposing the v0 verbs against the on-disk
layout. Read-write, no index yet, no embeddings. Use mark3labs/mcp-go for
the MCP surface and the standard library for filesystem.

Acceptance:

- [ ] `go.mod` initialized
- [ ] MCP server boots over stdio with `protocol_version` advertised
- [ ] `project.create / get / list / update` implemented
- [ ] `phase.add / list / update` implemented
- [ ] `task.create / claim / update / complete / list` implemented
- [ ] `artifact.link / list` implemented
- [ ] State machine enforced on `task.update`
- [ ] Smoke test: drive dossier's own corpus through the verbs and assert
  the resulting files match what's already on disk
- [ ] Single binary build with `go build`

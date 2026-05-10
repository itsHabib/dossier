---
id: prj_01J05M2WXG4F8Z7K3N9P5Q1R6T
slug: dossier
title: dossier — agent project protocol and reference mesh
status: active
created_at: 2026-05-10T00:00:00Z
updated_at: 2026-05-10T00:00:00Z
created_by: human:mh
---

# dossier

Agent-native project management. Two artifacts:

1. The **Agent Project Protocol (APP)** — a wire spec over MCP that any
   server can implement and any agent can call. Defines primitives
   (project, phase, task, artifact) and verbs (create, claim, update,
   complete, link).
2. The **dossier mesh** — a reference Go server that implements APP over
   a git-backed markdown corpus. Single binary. Storage layer is
   human-readable on disk (see [LAYOUT.md](../../../LAYOUT.md)).

## Why

Markdown task docs work in one head. They break across teams, orgs, and
years: bloat, drift from reality, hard to query. Jira solves query but
humans hate it and agents can't reason about it well.

dossier's bet: a small, opinionated PM protocol over MCP. Implementer
agents (ship, future tools) call it to drive work. Reader agents call it
to answer "what's going on with X". Humans interact through whatever
frontend they prefer — chat, terminal, web. The doc is still the artifact;
the protocol gives it structure agents can act on.

## Non-goals (v0)

- General-purpose document store (it's project management, opinionated)
- Cross-project dependency graph
- Multi-tenant auth
- Permissions model
- Sprints, estimates, burndowns

## References

- [PROTOCOL.md](../../../PROTOCOL.md) — wire spec
- [LAYOUT.md](../../../LAYOUT.md) — on-disk storage convention
- [README.md](../../../README.md) — project overview

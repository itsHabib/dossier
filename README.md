# dossier

Agent-native project management. A protocol and a reference mesh.

- **[PROTOCOL.md](PROTOCOL.md)** — the Agent Project Protocol (APP) spec.
  Defines the primitives (project, phase, task, artifact) and verbs
  (create, claim, update, complete, link) that any conforming MCP server
  exposes and any conforming agent can call.
- **mesh** — reference server implementing the protocol. Not yet built.
  Will be a single Go binary: MCP server + git-backed markdown store +
  index. Storage and search arrive after the protocol stabilizes.

## Why

Markdown task docs work great in a single project, in a single head. They
break across teams, orgs, and years: bloat, drift from reality, hard to
query. Jira and friends solve query but humans hate filling them out and
agents can't reason about them well.

dossier's bet: define a small protocol over MCP so any agent (implementers
like ship, manager-facing readers, future tools) can read and write project
state through a uniform surface. The doc is still the artifact; the
protocol gives it structure agents can act on.

## Layers

Each layer is standalone-useful. Build bottom-up.

1. **Protocol spec** — `PROTOCOL.md`. The contract.
2. **Reference mesh** — Go MCP server, git-backed markdown store. Implements
   the protocol against a real backend.
3. **Index + search** — semantic + keyword over the corpus. Powers reader
   agents.
4. **Conforming clients** — ship and others as implementers; a reader CLI
   or chat surface for managers.

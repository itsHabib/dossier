# dossier

Project memory for the solo developer. One markdown-on-disk corpus tracking
design docs, TDDs, and task notes across a portfolio, queryable through any
LLM via MCP. See [docs/vision.md](docs/vision.md) for the longer "what +
why + what we're explicitly not building."

## Quick start

```sh
# 1. Build + install the mesh binary
cargo install --path .

# 2. Pick (or create) a corpus directory
mkdir -p ~/dossier-corpus/.dossier   # the .dossier/ marker is all you need

# 3. Register the mesh with Claude Code as an MCP server
claude mcp add dossier -- "$(which dossier-mesh)" serve --corpus ~/dossier-corpus

# 4. Open a new Claude Code session
#    The verbs are now available: project.create, phase.add, task.create,
#    task.claim, task.update, task.complete, artifact.link, plus the
#    matching list / get reads.
```

Ask Claude to create a project for one of your repos and link a PR:

> Create a project in dossier for "tower" with slug `tower` and title "Tower —
> worktree observer". Add a phase `01-spec` titled "v0 spec". Then link
> PR #42 from `itsHabib/tower` as an artifact under that phase.

That's it. No `init` command needed — the write verbs scaffold the
directory tree as you go.

## Layout

A corpus is any directory with a `.dossier/` marker. Inside:

```
~/dossier-corpus/
  .dossier/
    config.toml          # reserved; may be empty
  projects/
    <project-slug>/
      project.md         # YAML frontmatter + markdown body
      phases/
        01-<phase-slug>.md
      tasks/
        <task-id>-<task-slug>.md
      artifacts.jsonl    # append-only
```

Full format in [LAYOUT.md](LAYOUT.md). The corpus is the source of truth
— humans grep and edit the markdown directly, and the mesh re-reads it
on every call.

## Verbs

| Read | Write |
| --- | --- |
| `project.list` | `project.create` |
| `project.get` | `project.update` |
| `phase.list` | `phase.add` |
| `task.list` | `phase.update` |
| `artifact.list` | `task.create` |
| | `task.claim` |
| | `task.update` |
| | `task.complete` |
| | `artifact.link` |

Data model in [PROTOCOL.md](PROTOCOL.md), including the task state machine.

## Develop

```sh
make check        # fmt-check + clippy --all-targets -- -D warnings + test
make fmt          # apply rustfmt
make test         # cargo test
make build        # debug build
make release      # release build
```

CI runs `make check` on every PR. Lint discipline + conventions live in
[CLAUDE.md](CLAUDE.md).

## Why this exists

A solo dev with a dozen side projects has the same recurring problem:
*where did I write that down?* The design doc for the auth migration
lives in one repo, the TDD for the data pipeline in another, and the
PRs are scattered across GitHub. dossier consolidates the project-state
plane in plain markdown that humans grep and LLMs query. The full
framing — and the explicit list of things we're *not* building — is in
[docs/vision.md](docs/vision.md).

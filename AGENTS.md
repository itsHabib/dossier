# AGENTS.md

See [CLAUDE.md](CLAUDE.md) for the authoritative project guide (architecture, conventions, lint discipline, develop commands, common gotchas).

## Cursor Cloud specific instructions

- **Toolchain**: The pre-installed Rust 1.83 is too old — `rmcp-macros` requires edition 2024 (Rust 1.85+). The update script runs `rustup default stable && rustup update stable` to ensure a recent toolchain is active. If you see `feature edition2024 is required`, the default toolchain is stale.
- **No external services**: This is a self-contained Rust binary with no databases, Docker, or network dependencies. `cargo test` creates temp dirs via `tempfile` — no setup needed.
- **Single CI command**: `make check` (= `fmt-check` + `clippy --all-targets -- -D warnings` + `cargo test --all-features`). Run before every push.
- **Testing the MCP server**: The binary communicates over stdio. Pipe JSON-RPC messages to `./target/debug/dossier --corpus <path> serve`. The in-repo dogfood corpus at the repo root (has `.dossier/` marker) works as a test corpus.
- **CLI subcommands**: `task_list`, `task_complete`, `task_update`, `artifact_link` work against the corpus directly (no MCP). Use `--corpus .` from the repo root to hit the dogfood corpus.
- **Corpus marker**: `FsStore::open` requires a `.dossier/` directory at the corpus root. The repo root already has one.

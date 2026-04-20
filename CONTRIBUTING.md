# Contributing to SynapsCLI

Rust-native AI agent runtime. Contributions welcome.

## Quick Start

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd SynapsCLI
cargo build --release
cargo test --release --lib
```

**Requirements:** Rust 1.80+, Linux or macOS

**Note:** 7 PTY tests (`tools::shell`) fail under parallel execution — pass with `cargo test --release --lib -- --test-threads=1`. Not a bug, just TTY contention.

## Branch Model

- `main` — stable, merge-only
- `dev` — active development, merge to main when validated
- `feat/*` — feature branches off dev

## Before Submitting

1. `cargo build --release` — 0 warnings
2. `cargo test --release --lib` — 224+ pass (PTY tests excluded from count)
3. Read `AGENTS.md` — especially the "Common Pitfalls" and "Key Patterns" sections
4. If adding a setting: update all 5 sync points (see AGENTS.md)
5. If touching cache logic: verify hit rate hasn't regressed (`SYNAPS_USAGE_LOG=1`)

## Code Style

- No `clippy::pedantic` — standard `cargo clippy` only
- `#[allow(dead_code)]` requires a comment explaining why
- Prefer `thiserror` for error types, `anyhow` for binary-level error handling
- One file per tool (`src/tools/*.rs`)
- Tests in `#[cfg(test)] mod tests {}` at file bottom

## What's Useful

- **Bug reports** with reproduction steps
- **Tool implementations** (new tools following the `Tool` trait in `src/tools/mod.rs`)
- **Themes** (add to `src/chatui/theme.rs`)
- **MCP server configs** (share working `mcp.json` setups)
- **Skills/plugins** (markdown-driven behavioral guidelines)

## License

By contributing, you agree your work is licensed under MIT.

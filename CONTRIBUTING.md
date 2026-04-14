# Contributing to Agent Runtime

Thanks for your interest in contributing. This is an open-source AI agent runtime built in Rust, and contributions of all kinds are welcome.

## Getting Started

```bash
git clone https://github.com/HaseebKhalid1507/SynapsCLI.git
cd agent-runtime
cargo build
cargo test
```

**Requirements:** Rust 1.70+, a Unix-like OS (Linux/macOS)

## How to Contribute

### Bug Reports

Open an [issue](https://github.com/HaseebKhalid1507/SynapsCLI/issues/new?template=bug_report.md) with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Your OS, Rust version, and version info

### Feature Requests

Open an [issue](https://github.com/HaseebKhalid1507/SynapsCLI/issues/new?template=feature_request.md) describing:
- The problem you're trying to solve
- Your proposed solution
- Any alternatives you've considered

### Pull Requests

1. Fork the repo and create a branch from `main`
2. Make your changes in focused, atomic commits
3. Add tests for new functionality
4. Ensure `cargo test`, `cargo build`, and `cargo clippy` all pass
5. Open a PR with a clear description of what and why

### Code Style

- Follow standard Rust conventions (`rustfmt` defaults)
- Run `cargo clippy` before submitting — no new warnings
- Keep functions focused — if it's doing too much, split it
- Error handling: prefer `Result` over `unwrap()` in production paths
- Add doc comments to public APIs

## Project Structure

```
src/
├── chatui/         # TUI interface (app, draw, theme, markdown, highlight)
├── runtime.rs      # API client, streaming, agentic tool loop
├── tools.rs        # Tool trait + implementations
├── watcher.rs     # Autonomous agent supervisor
├── agent.rs        # Headless agent worker
├── watcher_types.rs  # Watcher config and type definitions
├── auth.rs         # OAuth + API key authentication
├── mcp.rs          # MCP client (lazy loading)
├── skills.rs       # On-demand skill loading
├── session.rs      # Session persistence
└── config.rs       # Configuration management
```

## Adding a New Tool

Implement the `Tool` trait:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String>;
}
```

Register it in `ToolRegistry::new()` in `src/tools.rs`.

## Adding a Watcher Trigger

New trigger modes (cron, file-watch, webhook) are on the roadmap. If you want to work on one, open an issue first to discuss the approach.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
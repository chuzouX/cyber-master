# Repository Guidelines

## Project Structure & Module Organization

This is a Rust 2021 workspace. The executable entry point is `crates/cyber-app/src/main.rs` and builds as `cyber`. Shared behavior is divided by responsibility: `cyber-core` handles configuration and project context, `cyber-agent` contains providers and the agent loop, `cyber-tui` owns the Ratatui interface, and `cyber-mcp`, `cyber-skills`, `cyber-tools`, `cyber-workflow`, and `cyber-storage` provide supporting subsystems. Keep code inside the crate that owns its concern and preserve the dependency direction documented in `docs/DESIGN.md`. Default TOML resources live in `crates/cyber-core/assets/`; design and progress notes live in `docs/`.

Unit tests normally sit beside implementation in `#[cfg(test)]` modules. Cross-module integration tests belong in a crate-level `tests/` directory, as in `crates/cyber-agent/tests/mock_roundtrip.rs`.

## Build, Test, and Development Commands

- `cargo build` compiles the full workspace for development.
- `cargo run -p cyber-app -- --mock` launches an offline-friendly smoke-test mode.
- `cargo build --release --locked` produces the optimized binary using the committed lockfile.
- `cargo test --workspace` runs all unit and integration tests.
- `cargo fmt --all -- --check` verifies standard Rust formatting.
- `cargo clippy --workspace --all-targets -- -D warnings` treats every lint warning as a failure.

Run the formatting, test, and Clippy commands before opening a pull request.

## Coding Style & Naming Conventions

Use `rustfmt` defaults (four-space indentation) and idiomatic Rust naming: `snake_case` for modules, functions, and files; `CamelCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants. Prefer focused modules and typed errors over stringly typed failures. Add dependencies at the workspace level when multiple crates share them, then reference them with `{ workspace = true }`.

## Testing Guidelines

Name tests after observable behavior, for example `rejects_empty_provider_name`. Use `#[tokio::test]` for async paths and mocks for provider/network behavior. Add regression coverage with every bug fix. No numeric coverage threshold is published; prioritize configuration merging, tool guards, streaming parsers, and state transitions.

## Commit & Pull Request Guidelines

Recent history uses short, imperative prefixes such as `feat:`, `fix:`, and `release:`. Keep each commit scoped to one logical change. Pull requests should explain the problem and solution, list verification commands, and link related issues. Include screenshots or terminal captures for TUI changes and call out configuration or compatibility impacts.

## Security & Configuration

Never commit API keys, `.env` files, local `.cyber/` state, logs, or generated `target/` output. Use the templates in `crates/cyber-core/assets/` when documenting configuration, and keep real credentials in user-local configuration only.

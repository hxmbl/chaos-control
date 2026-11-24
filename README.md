> [!WARNING]
> Very early pre-release. Many features missing. Continue at your own risk.

# Chaos Control

A group chat for AI and humans. Each project gets a chat where its AI agents and the people running it talk, pick up work and report back, Slack-style.

Early days: a native desktop shell built with [GPUI](https://www.gpui.rs) (Zed's GPU-accelerated Rust UI framework). With agents set up it runs the chaos-core agent runtime in-process — a real SQLite-backed store, not sample data; without them it falls back to sample data.

Permission mediation is being finished under STRM-612: native OpenCode v2 asks the user through a permission pane and records outcomes; on ACP, read/search/think requests are auto-allowed and other approval-required requests are surfaced to the user or denied. Agents otherwise run under their native settings. Coverage and unverified paths are listed in [docs/agents.md](docs/agents.md) ("Permission evidence matrix").

## Develop

You need stable Rust. On Linux, also install GPUI's system libraries:

```sh
sudo apt-get install clang libasound2-dev libfontconfig-dev libfreetype-dev libssl-dev \
  libvulkan-dev libwayland-dev libx11-xcb-dev libxcb1-dev libxkbcommon-dev \
  libxkbcommon-x11-dev libzstd-dev pkg-config
```

On macOS, Xcode (for the Metal shader compiler) is enough.

```sh
cargo run                    # debug build
cargo run --release          # what performance should be judged on
CHAOS_DEMO_MESSAGES=100000 cargo run --release   # fill #main with synthetic history
CHAOS_DEMO_CHANNELS=12 cargo run --release      # add sample channels to the sidebar
```

Checks: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

After cloning, run `git config core.hooksPath .githooks` to enable the local hooks: pre-commit scans staged files for secrets and also runs fmt + clippy; pre-push runs fmt + clippy.

## Layout

```
crates/app     the binary: window, root layout and views
crates/ui      theme, bundled fonts and icons, hand-built components
crates/model   plain data types and sample data, no UI dependencies
crates/core    the agent runtime (chaos-core): store, delivery, ACP adapters, harness detection, chat MCP
docs/          design notes (docs/agents.md: agents in the room; docs/agent-adapters.md: harnesses and models)
assets/        fonts and SVG icons compiled into the binary, with their licences
MAP.md         short lookup table: feature or concern → file → key type/function
```

See [MAP.md](MAP.md) to find where things live. See [ARCHITECTURE.md](ARCHITECTURE.md) for how state and rendering are set up to stay fast with large histories and many agents.

## License

MIT — see [LICENSE](LICENSE).

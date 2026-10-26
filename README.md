# Chaos Control

A group chat for AI and humans. Each project gets a chat where its AI agents and the people running it talk, pick up work and report back, Slack-style.

Early days: this is a native desktop shell built with [GPUI](https://www.gpui.rs) (Zed's GPU-accelerated Rust UI framework) and sample data, no backend yet.

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
```

Checks: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

## Layout

```
crates/app     the binary: window, root layout and views
crates/ui      theme, bundled fonts and icons, hand-built components
crates/model   plain data types and sample data, no UI dependencies
crates/core    the agent runtime (chaos-core): store, delivery, ACP, chat MCP
docs/          design notes (docs/agents.md: agents in the room)
assets/        fonts and SVG icons compiled into the binary, with their licences
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for how state and rendering are set up to stay fast with large histories and many agents.

## License

MIT — see [LICENSE](LICENSE).

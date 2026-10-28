//! Chaos Control's agent runtime. No GPUI: the app is one client of this crate.
//!
//! - [`store`]: SQLite (messages, members, cursors, triggers, batches, obligations).
//! - [`engine`]: the delivery and wake engine (sync, clock-driven).
//! - [`policy`]: room dampers (hop budget, ping-pong, rate limit).
//! - [`adapter`]: the `AgentAdapter`/`AgentSession` boundary and `AgentEvent`.
//! - [`acp`]: the generic ACP adapter (v1, plus the v2 draft).
//! - [`mcp`]: the chat MCP server and the stdio bridge.
//! - [`permission`]: permissions by where the agent runs (host: ask the
//!   humans, non-blocking; own VM: approve).
//! - [`host`]: the in-process runtime behind the [`host::Core`] handle.
//! - [`fake`]: a scripted ACP agent for tests.
//!
//! The rules are written up in `docs/agents.md`.

pub mod acp;
mod acp_v2;
pub mod adapter;
pub mod engine;
pub mod fake;
pub mod host;
pub mod mcp;
pub mod notify;
pub mod permission;
pub mod policy;
pub mod store;
pub mod types;

//! Native OpenCode V2 adapter (STRM-612).
//!
//! Connects to an OpenCode v2 server (`opencode serve`) over HTTP/SSE instead
//! of speaking ACP (`opencode acp`). This is intentionally separate from the
//! ACP adapters so existing ACP paths remain intact.
//!
//! Status: initial scaffold for the STRM-612 ship-today slice. Capabilities
//! and enforcement guarantees must be reported honestly; unsupported or
//! unverified behavior must not be presented as protected.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::{
    adapter::{
        AgentAdapter, AgentError, AgentErrorKind, AgentEvent, AgentSession, Caps, ChatToolsTransport,
        EventSink, OpenSession, Probe, SessionInfo, TurnOutcome,
    },
    types::BatchId,
};

/// How to reach an OpenCode v2 server.
#[derive(Clone, Debug)]
pub struct OpenCodeV2Config {
    /// `opencode` binary, used to start a server when `server_url` is absent.
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Connect to an existing OpenCode v2 server instead of starting one.
    pub server_url: Option<String>,
    /// Password for HTTP basic auth (user `opencode`), if required.
    pub server_password: Option<String>,
}

pub struct OpenCodeV2Adapter {
    config: OpenCodeV2Config,
}

impl OpenCodeV2Adapter {
    pub fn new(config: OpenCodeV2Config) -> Self {
        OpenCodeV2Adapter { config }
    }
}

impl AgentAdapter for OpenCodeV2Adapter {
    fn describe(&self) -> String {
        match &self.config.server_url {
            Some(url) => format!("opencode-v2: {url}"),
            None => format!(
                "opencode-v2: {} {}",
                self.config.command.display(),
                self.config.args.join(" ")
            ),
        }
    }

    fn probe(&self) -> BoxFuture<'_, Probe> {
        let config = self.config.clone();
        Box::pin(async move {
            if config.server_url.is_some() {
                // Reachability is verified on open; probing a URL here would add
                // network side effects. Report plausible so configured endpoints
                // are selectable.
                return Probe::Plausible;
            }
            if crate::acp::resolve_command(&config.command).is_some() {
                Probe::Plausible
            } else {
                Probe::Missing {
                    why: Arc::from(format!("`{}` not found on PATH", config.command.display())),
                }
            }
        })
    }

    fn open(&self, req: OpenSession) -> BoxFuture<'_, Result<Arc<dyn AgentSession>, AgentError>> {
        let config = self.config.clone();
        Box::pin(async move { open_session(config, req).await })
    }
}

enum Cmd {
    Deliver {
        turn: BatchId,
        prompt: Arc<str>,
        done: tokio::sync::oneshot::Sender<Result<(), AgentError>>,
    },
    Interrupt,
    Close,
}

struct OpenCodeV2Session {
    info: SessionInfo,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl AgentSession for OpenCodeV2Session {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn deliver(&self, turn: BatchId, prompt: Arc<str>) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let (done, rx) = tokio::sync::oneshot::channel();
            self.cmd
                .send(Cmd::Deliver { turn, prompt, done })
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session is closed"))?;
            rx.await
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session is closed"))?
        })
    }

    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            self.cmd
                .send(Cmd::Interrupt)
                .map_err(|_| AgentError::new(AgentErrorKind::Transport, "session is closed"))
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.cmd.send(Cmd::Close);
        })
    }
}

async fn open_session(
    _config: OpenCodeV2Config,
    _req: OpenSession,
) -> Result<Arc<dyn AgentSession>, AgentError> {
    // STRM-612 today-slice scaffold: native OpenCode v2 server connection,
    // session create/resume, event streaming, tool/approval handling and
    // lifecycle are not implemented yet. Fail honestly rather than claiming an
    // ACP-backed or unverified path.
    Err(AgentError::new(
        AgentErrorKind::Protocol,
        "native OpenCode v2 adapter is not implemented yet (STRM-612 scaffold); \
         use the ACP path for now, or configure a verified native implementation",
    ))
}

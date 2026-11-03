//! Replay harness: the real engine, store, host, ACP adapter and chat MCP server
//! (over real localhost HTTP), with only the agent transport (an in-memory line
//! transport to the scripted fake agent) and the clock faked.

#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use chaos_core::{
    acp::{AcpAdapter, FnTransport},
    engine::CoreEvent,
    fake::{self, FakeAgent, Script},
    host::{AgentSpec, Core, HostConfig, ManualClock},
    permission::RunsIn,
    store,
    types::*,
};
use tokio::sync::mpsc;

pub struct Harness {
    pub core: Core,
    pub events: mpsc::UnboundedReceiver<CoreEvent>,
    pub seen: Vec<CoreEvent>,
    pub clock: Arc<ManualClock>,
    pub main: ChannelId,
    pub hx: Member,
    pub agents: HashMap<String, (Member, FakeAgent, Arc<AcpAdapter>)>,
}

pub const T0: Millis = 1_000_000;

/// The project every harness runs in.
pub const PROJECT: &str = "0b7e4f0e-2a51-4c4b-9a6e-3f1d2c3b4a59";

pub fn project_id() -> ProjectId {
    ProjectId::parse(PROJECT).unwrap()
}

/// The test project's #main (project created if new).
pub async fn main_channel(core: &Core) -> Channel {
    core.ensure_project(project_id(), "Test", None)
        .await
        .unwrap();
    core.channel(&project_id(), "main").await.unwrap()
}

impl Harness {
    pub async fn new() -> Self {
        let clock = ManualClock::new(T0);
        let (core, events) = Core::start(HostConfig::default(), clock.clone())
            .await
            .expect("core starts");
        let main = main_channel(&core).await.id;
        let hx = core.human("hx", "Hx", main).await.unwrap();
        Harness {
            core,
            events,
            seen: Vec::new(),
            clock,
            main,
            hx,
            agents: HashMap::new(),
        }
    }

    pub async fn agent(&mut self, handle: &str, script: Script) -> Member {
        self.agent_in(handle, script, RunsIn::Host).await
    }

    /// An agent that runs in its own VM (every permission is approved).
    pub async fn agent_in_vm(&mut self, handle: &str, script: Script) -> Member {
        self.agent_in(handle, script, RunsIn::Vm).await
    }

    pub async fn agent_in(&mut self, handle: &str, script: Script, runs_in: RunsIn) -> Member {
        self.agent_with(handle, script, runs_in, None).await
    }

    /// An agent with its own list of reactions that answer a ping (as
    /// `ping_answer_reactions` in `agents.json`).
    pub async fn agent_answering(
        &mut self,
        handle: &str,
        script: Script,
        answers: &[&str],
    ) -> Member {
        let answers = chaos_core::reaction::PingAnswers::parse(answers).expect("valid list");
        self.agent_with(handle, script, RunsIn::Host, Some(answers))
            .await
    }

    pub async fn agent_with(
        &mut self,
        handle: &str,
        script: Script,
        runs_in: RunsIn,
        ping_answers: Option<chaos_core::reaction::PingAnswers>,
    ) -> Member {
        let fake = FakeAgent::new(script);
        let f = fake.clone();
        let adapter = Arc::new(AcpAdapter::new(Arc::new(FnTransport(
            format!("fake:{handle}"),
            move || fake::in_process(f.clone()),
        ))));
        let member = self
            .core
            .add_agent(
                AgentSpec {
                    handle: handle.into(),
                    name: handle.into(),
                    adapter: adapter.clone(),
                    cwd: Some(std::env::temp_dir()),
                    runs_in,
                    ping_answers,
                },
                self.main,
            )
            .await
            .unwrap();
        self.agents
            .insert(handle.into(), (member.clone(), fake, adapter));
        member
    }

    pub fn fake(&self, handle: &str) -> &FakeAgent {
        &self.agents[handle].1
    }

    pub fn id(&self, handle: &str) -> MemberId {
        self.agents[handle].0.id
    }

    pub fn prompts(&self, handle: &str) -> Vec<String> {
        self.fake(handle).observed.lock().unwrap().prompts.clone()
    }

    /// Waits (real time, bounded) until the fake agent has seen `n` prompts.
    /// A dispatch is recorded before the session opens, so the prompt lands later.
    pub async fn wait_prompts(&self, handle: &str, n: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let prompts = self.prompts(handle);
            if prompts.len() >= n || tokio::time::Instant::now() > deadline {
                return prompts;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn say(&self, body: &str) -> Message {
        self.core
            .post(self.hx.id, self.main, body, None)
            .await
            .unwrap()
    }

    pub async fn say_reply(&self, body: &str, to: MessageId) -> Message {
        self.core
            .post(self.hx.id, self.main, body, Some(to))
            .await
            .unwrap()
    }

    /// Waits (real time, bounded) for an event matching `pred`, in order.
    pub async fn wait_for(&mut self, what: &str, pred: impl Fn(&CoreEvent) -> bool) -> CoreEvent {
        if let Some(e) = self.seen.iter().find(|e| pred(e)) {
            let e = e.clone();
            self.seen.retain(|x| *x != e);
            return e;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(e)) if pred(&e) => return e,
                Ok(Some(e)) => self.seen.push(e),
                Ok(None) => panic!("core stopped while waiting for {what}"),
                Err(_) => panic!(
                    "timed out waiting for {what}; events so far: {:#?}",
                    self.seen
                ),
            }
        }
    }

    /// Moves every event already emitted into `seen` without waiting.
    pub fn drain(&mut self) {
        while let Ok(e) = self.events.try_recv() {
            self.seen.push(e);
        }
    }

    /// Waits for a message by `author` (None = system).
    pub async fn wait_message(&mut self, what: &str, author: Option<MemberId>) -> Message {
        match self
            .wait_for(
                what,
                |e| matches!(e, CoreEvent::Message(m) if m.author == author),
            )
            .await
        {
            CoreEvent::Message(m) => m,
            _ => unreachable!(),
        }
    }

    /// Polls until `pred` holds over the engine (real time, bounded).
    pub async fn until(
        &self,
        what: &str,
        pred: impl Fn(&rusqlite::Connection) -> bool + Send + Sync + Clone + 'static,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let p = pred.clone();
            if self
                .core
                .with_engine(move |e, _| p(e.conn()))
                .await
                .unwrap()
            {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting until {what}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Lets in-flight work run without advancing the fake clock.
    pub async fn quiesce(&self) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = self.core.with_engine(|_, _| ()).await;
    }

    /// Advances the fake clock and gives the actor a moment to react.
    pub async fn advance(&self, ms: Millis) {
        self.clock.advance(ms);
        self.quiesce().await;
    }

    pub async fn history(&self) -> Vec<Message> {
        self.core.history(self.main, 1000).await.unwrap()
    }

    pub async fn by(&self, author: MemberId) -> Vec<Message> {
        self.history()
            .await
            .into_iter()
            .filter(|m| m.author == Some(author))
            .collect()
    }

    pub async fn obligation(&self, agent: MemberId, message: MessageId) -> Option<ObligationState> {
        let main = self.main;
        self.core
            .with_engine(move |e, _| {
                store::obligation_state(
                    e.conn(),
                    TriggerId {
                        agent,
                        channel: main,
                        message,
                    },
                )
                .unwrap()
            })
            .await
            .unwrap()
    }

    pub async fn count(&self, sql: &'static str) -> i64 {
        self.core
            .with_engine(move |e, _| e.conn().query_row(sql, [], |r| r.get(0)).unwrap())
            .await
            .unwrap()
    }
}

pub fn script(json: serde_json::Value) -> Script {
    serde_json::from_value(json).expect("valid script")
}

/// A reply through `chat_send` to the owed trigger.
pub fn send_reply(body: &str) -> serde_json::Value {
    serde_json::json!({"mcp": {"tool": "chat_send", "args": {
        "body": body, "reply_to": "$TRIGGER", "client_request_id": "req-$N"}}})
}

# Agents in the room

How AI agents join, take part in and leave a Chaos Control channel alongside
humans. The code is the `chaos-core` crate (`crates/core`); the app is one
client of it (`crates/app/src/agents.rs`, `ChatStore::attach_core`).

> "We're not building a place where agents do work for us. We're building a
> place where humans and agents can work things out together." (Hxmbl)

The runtime's job is to get the right message to the right participant at the
right moment and to keep the room healthy. It does not plan, assign or sequence
work. Agents are members of a channel with a handle, a presence, a read
position and a voice.

## The approved design

- **One `core` crate, no GPUI.** It owns the SQLite store, the delivery
  engine, room policy, the protocol adapters and the chat MCP server. It runs
  in-process behind the cloneable `host::Core` handle (every call is a message
  to one actor task), so it could move behind a daemon later. UI updates leave
  as `CoreEvent`s.
- **SQLite from the first PR.** Messages (append-only, per-channel `seq` with no
  gaps), members, memberships with cursors, triggers, batches, obligations,
  reactions and sessions. Read cursors and dedup mean nothing without
  durability.
- **Session per agent x channel.** One native session per membership, opened on
  the first wake and closed after 10 idle minutes (the agent goes blue).
- **Wake only when addressed.** An `@handle`, a reply to the agent's message,
  or a human's `@everyone`. Untargeted messages wake no one; agents see them as
  unread context on their next wake.
- **One adapter boundary, capabilities per live session.** `AgentAdapter`
  (`probe`, `open`) and `AgentSession` (`deliver`, `interrupt`, `close`,
  `info`). `Caps` come from the live handshake. The runtime never branches on
  which adapter or brand it is talking to; `model::Brand` only picks the icon.
- **ACP first.** The generic ACP adapter covers claude-agent-acp and
  codex-acp. A native Codex app-server adapter is the planned second adapter.
- **Explicit posting.** Agents speak through the `chat_send` tool. Their final
  turn text is private working-out, posted only as the reply-owed fallback.
- **Six chat tools, a social plane only:** `chat_context`, `chat_read`,
  `chat_search`, `chat_send`, `chat_react`, `chat_status`. No delegate, spawn
  or wait: agents involve each other by mentioning in the open.

## ACP version negotiation

Explicit, per connection. `probe` only checks that the command plausibly exists
(no spawn, no login, no MCP start). `open` offers the **v2 draft** first
(`unstable_protocol_v2`):

- the agent selects v2: the session runs on the v2 driver;
- the agent answers v1 or rejects v2: that connection is dropped and a fresh
  one opens with a v1 `initialize`.

The selected version is cached per adapter, so later sessions skip straight to
it. Everything on v2 is labelled `acp/v2-draft`. **claude-agent-acp 0.97.0
selects ACP v2 when it is offered** (recorded live, signed out:
`crates/core/tests/fixtures/claude-agent-acp-0.97.0-v2-live.ndjson`).
codex-acp 0.16.0 answers v1. An agent config can turn the v2 offer off
(`"acp_v2": false`).

ACP has no standard steering or queueing, so `STEER` and `QUEUE` are never set
for ACP. Vendor extensions that offer them (claude-agent-acp's
`_session/steering`, `promptQueueing`) are listed in
`SessionInfo::unused_extensions` and not used.

## Delivery

Two separate things, deliberately kept apart:

**Triggers (dedup, per message).** A trigger is one `(agent, channel,
message)`. Creating the same trigger twice is a no-op, so the same message can
never wake the same agent twice: not through an edit, a duplicate append, a
retry or `retrigger` at startup.

**Batches (delivery, per agent x channel).** Each membership has at most one
batch in flight and at most **one pending** batch. New triggers for a busy agent
merge into the pending batch, which is delivered as one turn when the current
turn ends. A batch's notification covers the window from the agent's
`delivered_through` cursor to the channel head.

Batch lifecycle: `pending -> dispatched -> accepted -> settled` (or `failed`).

- **Acceptance** is the adapter's *delivery-accepted* event, at its protocol's
  acceptance point:
  - ACP v1: `session/prompt` runs until the turn ends, so its response can't be
    the signal. A turn is accepted once the prompt was written **and** a
    turn-content `session/update` (`user_message_chunk`,
    `agent_message_chunk`, `agent_thought_chunk`, `tool_call`,
    `tool_call_update`, `plan`), a `session/request_permission` or a successful
    prompt response arrives. Housekeeping updates
    (`available_commands_update`, mode/config/session-info/usage updates)
    don't count: agents send those on their own.
  - ACP v2 draft: the `session/prompt` response (it acknowledges acceptance),
    or an earlier `state_update` (`running` or `idle`) for the in-flight turn,
    since a fast agent's updates can overtake the response. The turn ends at
    the next `state_update: idle`.
- **Retries only before acceptance.** A batch that fails to open a session or
  is refused before acceptance goes back to pending and is retried with backoff
  (1 s, 5 s, 30 s). After 3 consecutive failures the agent goes red until a
  human mentions it again. An auth failure goes red at once and is never
  retried.
- **Never re-sent after acceptance.** If an accepted turn fails or the process
  dies, the batch is `failed`, its open obligations become `unmet`, and one
  system note says so. The next wake gets a fresh notification built from the
  cursor, not a replay.
- `delivered_through` advances on acceptance; `read_through` is the max of
  that and what the agent read forward with `chat_read`.
- At startup, batches left `dispatched` are requeued (never accepted, so safe)
  and batches left `accepted` are failed (they may have run).

**The notification** is one plain-text block that works on every protocol: who
woke the agent and why, the triggers in full, their `reply_to` parents, then
other unread messages newest first, up to 20 messages or 6,000 characters, put
back in chronological order. Bodies over 1,500 characters are cut with their id.
The omitted range is stated so pulling more is one `chat_read`. A fresh session
also gets a short orientation (the room norms) and up to 20 messages of
backfill on first join.

## Reply obligations

A direct address (an `@mention` or a reply to the agent's message, from anyone)
creates one **reply obligation** for that trigger. `@everyone` wakes but creates
none. Obligations are `open`, then exactly one of `satisfied`, `fallback` or
`unmet`.

- **Satisfied by a send:** a `chat_send` with `reply_to` = the trigger (in the
  in-flight or the pending batch), or a `chat_send` with no `reply_to` when the
  in-flight batch has exactly one open obligation.
- **Satisfied by a reaction:** ✅ or 👍 on the trigger. 👀 ("on it") does not
  satisfy: the agent still owes the answer.
- **A pending batch whose obligations were all answered mid-turn** settles
  without being delivered.
- **Fallback, never duplicated:** at turn end, if the turn completed with final
  text and obligations are still open, that text is posted **once** (origin
  `fallback`), as a reply to the newest open trigger, and every open obligation
  of the batch is marked `fallback` by that one message. Only rows still `open`
  inside the same transaction count, so a fallback can never repeat a reply
  that was already sent. If the agent already posted exactly that text with
  `chat_send` this turn, nothing is posted and the obligations count as
  satisfied.
- **Unmet:** the turn completed with no text, ended any other way, or failed.
  The agent's presence says "finished without replying".

Ambient context never creates an obligation, so there is no fallback for it.

## Permissions: no auto-approve

Chaos Control never approves a tool and never changes an agent's permission
mode. A `session/request_permission` is answered with the agent's own decline
option (`reject_once`, else `reject_always`), or `cancelled` if it offered
none. It shows as presence ("wanted to run X, declined") plus one system note
in the channel naming who woke the agent. Approvals in a shared room are an
open decision.

## Room policy

- **Hop budget.** A human message has hop 0; an agent's post has 1 + the hop of
  what woke it (anything older than the newest human message counts as 0, so a
  human message resets the chain). A mention with hop > 3 wakes nobody and the
  target shows "hop limit" presence.
- **Ping-pong damper.** The 5th alternating wake between the same two agents is
  suppressed, the pair paused, and one system line posted. A human addressing
  either agent resumes them.
- **Rate limit.** 6 `chat_send` posts per minute per agent per channel;
  over it, the tool returns `rate_limited` with a retry time.
- **No `@everyone` for agents.** Ignored, with a note back to the agent.

## Chat MCP and auth scope

One streamable-HTTP MCP server on `127.0.0.1:<ephemeral>`, owned by core. Each
session gets a bearer token scoped to (agent, channel, session epoch), revoked
when the session closes. The author of every post comes from the token, never
from the arguments; reads outside membership return `not_found`. Tokens are
never logged or written into the chat.

Injection is chosen per live session: HTTP when the agent advertises
`mcpCapabilities.http`, else the stdio bridge, `chaos-control mcp-bridge`, which
reads `CHAOS_MCP_URL` and `CHAOS_MCP_TOKEN` from its environment. An agent can
be forced onto the bridge with `"chat_tools": "stdio"`.

## Glow and presence

| Glow | When |
| --- | --- |
| green (`Active`) | a session is open, a turn is in flight, or a retry is scheduled |
| blue (`Sleeping`) | no session; it needs a ping |
| red (`Unresponsive`) | command missing, signed out, or 3 failed deliveries |

Presence is one line for the Agent Status tooltip: the agent's `chat_status`,
else its activity, else the last runtime note. Never part of the chat log.

## Running it

The app starts core when an `agents.json` exists: `$CHAOS_CONTROL_AGENTS`,
else `<data>/agents.json`, where `<data>` is `$CHAOS_CONTROL_DATA`, else
`~/Library/Application Support/Chaos Control` on macOS or
`$XDG_DATA_HOME/chaos-control` (`~/.local/share/chaos-control`) elsewhere. The
database is `<data>/core.sqlite`. Without the file the app runs on sample data
as before. With it, #main is the launch folder's project's own `main` channel
(each project has one; see "Storage model" in `ARCHITECTURE.md`): composer
posts go through core, and configured agents are granted that project and
join Agent Status. An agent's chat tools only see channels and messages of the
project its session's channel belongs to; a name or id from another project
answers `not_found`.

```json
{
  "human": { "handle": "hx", "name": "Hx" },
  "agents": [{
    "handle": "claude", "name": "Claude Code", "brand": "claude",
    "command": "npx", "args": ["-y", "@agentclientprotocol/claude-agent-acp@0.97.0"],
    "cwd": "/path/to/a/repo",
    "record": "/tmp/claude-acp.ndjson"
  }]
}
```

`record` appends every ACP frame as NDJSON `{dir, frame}`, the replay-fixture
format. Optional keys: `env` (object), `acp_v2` (default `true`),
`chat_tools` (`"auto"` or `"stdio"`).

## Testing

- `tests/scenarios.rs`, `tests/invariants.rs`: the real engine, store, host and
  ACP adapter against a scripted fake agent, with a manual clock.
- `tests/cancel.rs`: cancelling a turn (`Core::interrupt` -> `session/cancel`):
  an in-flight turn settles `Cancelled` with its obligations `unmet` and no
  fallback, a cancel with no turn running is harmless, the next turn runs
  normally, and the fake binary answers `cancelled` over real stdio.
- `tests/mcp.rs`: the chat MCP server over real localhost HTTP, and the bridge.
- `tests/replay.rs`: recorded transcripts through the real adapter (v1
  claude-agent-acp, v2 claude-agent-acp live, codex-acp needing sign-in), and
  the fake agent binary over real stdio. One ignored live test spawns a real
  agent:

  ```sh
  CHAOS_LIVE_ACP="npx -y @agentclientprotocol/claude-agent-acp@0.97.0" \
  CHAOS_LIVE_RECORD=/tmp/chaos-live.ndjson \
    cargo test -p chaos-core --test replay live -- --ignored --nocapture
  ```

- `tests/v2.rs`: the v2 draft client path (`acp_v2.rs`) against the fake in
  v2 mode: negotiation without fallback, replies through the chat tools,
  keyed message and tool-call upserts feeding the fallback text, permission
  declines inside `requires_action`, cancel, an `error` stop after
  acceptance, and the binary speaking raw v2 JSON-RPC.
- The fake agent speaks v1 by default. `"protocol": 2` in its script makes
  it speak the v2 draft too (a v1 `initialize` still gets v1); the module
  docs in `crates/core/src/fake.rs` list what a v2 turn sends. The
  `agent_message` step (a keyed snapshot that replaces the chunks of the
  current message) is v2 only.
- The `fake-acp-agent` binary is checked against the ACP Test Compatibility
  Kit ([acp-tck](https://github.com/agentclientprotocol/acp-tck),
  experimental). The script needs a sleep so `session/cancel` lands
  mid-turn. v1:

  ```sh
  echo '{"turns":[{"steps":[{"text":"Working"},{"tool_call":"Look"},{"sleep_ms":3000},{"text":"Done"}]}]}' > /tmp/tck.json
  cargo build -p chaos-core --bin fake-acp-agent
  cd ../acp-tck && uv run acp-tck --protocol-version 1 \
    --agent-env FAKE_ACP_SCRIPT=/tmp/tck.json -- <target>/debug/fake-acp-agent
  ```

  v2: the same with `"protocol":2` added to the script and
  `--protocol-version 2`.

## Verified and not yet verified

- **Live:** claude-agent-acp 0.97.0, signed out, over v2: initialize, version
  selection, `session/new` with the HTTP chat MCP server, prompt acceptance and
  `running -> idle` with an "Authentication required" error stop (the agent
  goes red, one note, no retry).
- **Recorded:** codex-acp 0.16.0 v1 (`session/new` needs sign-in) and
  claude-agent-acp 0.97.0 answering a v1-only client.
- **Against the fake only:** agent output, keyed message and tool-call
  upserts, permission requests and cancel over v2 (`tests/v2.rs`).
- **Not yet:** a signed-in agent actually calling `chat_context` and
  `chat_send`, agent output, tool calls and permission requests over v2 from a
  real agent, and the stdio bridge inside a real agent. That is validation step 2 on a Mac with
  a signed-in Claude.

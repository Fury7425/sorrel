//! ACP driver for long-tail agents (Cursor, Gemini, OpenCode): one process per
//! session, JSON-RPC 2.0 on stdio, protocol v1.
//!
//! Wire names checked against `agent-client-protocol-schema` 1.9.1, the
//! official schema crate:
//! - `initialize` {protocolVersion: 1, clientCapabilities} — file system and
//!   terminal capabilities are declined, so the agent uses its own tools.
//! - `session/new` {cwd, mcpServers}, or `session/load` when resuming and the
//!   agent supports it (the history it replays is dropped; the log has it).
//! - `session/prompt` {sessionId, prompt}: its response's `stopReason` ends
//!   the turn. `session/cancel` interrupts.
//! - `session/update` notifications: agent_message_chunk, agent_thought_chunk,
//!   tool_call, tool_call_update, plan.
//! - `session/request_permission`, answered with outcome `selected`
//!   {optionId} or `cancelled`.

use std::{collections::HashMap, collections::VecDeque, path::Path, time::Duration};

use proto::{
    Access, AgentEvent, McpServer, PermChoice, Provider, StopReason, TodoItem, TodoStatus,
    ToolKind, ToolStatus,
};
use serde_json::{Value, json};
use tokio::{
    process::Child,
    sync::{mpsc, oneshot},
    time::{Instant, sleep_until},
};

use crate::{
    DriverCommand, SessionConfig, StderrTail, blob, clip, fail, mode_prefix,
    rpc::{self, Incoming, Peer},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How to start an agent's ACP server.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub bin: &'static str,
    pub args: Vec<String>,
    /// Where API-key mode puts the key.
    pub key_env: &'static str,
    /// What to tell a signed-out user.
    pub sign_in: &'static str,
}

/// Defaults per agent; `SORREL_<AGENT>_BIN` and `SORREL_<AGENT>_ARGS` override them.
// ponytail: Gemini's ACP flag has moved between releases; override with SORREL_GEMINI_ARGS if it changes again.
pub fn launch(provider: Provider) -> Launch {
    let (bin, args, key_env, sign_in): (&'static str, &[&str], &'static str, &'static str) =
        match provider {
            Provider::Cursor => (
                "agent",
                &["acp"],
                "CURSOR_API_KEY",
                "Run `agent login` in a terminal.",
            ),
            Provider::Gemini => (
                "gemini",
                &["--experimental-acp"],
                "GEMINI_API_KEY",
                "Run `gemini` once in a terminal to sign in.",
            ),
            Provider::OpenCode => (
                "opencode",
                &["acp"],
                "OPENCODE_API_KEY",
                "Run `opencode auth login` in a terminal.",
            ),
            Provider::Claude | Provider::Codex => ("", &[], "", ""),
        };
    let args = std::env::var(format!(
        "SORREL_{}_ARGS",
        provider.key().to_ascii_uppercase()
    ))
    .map(|args| args.split_whitespace().map(str::to_owned).collect())
    .unwrap_or_else(|_| args.iter().map(|arg| (*arg).to_owned()).collect());
    Launch {
        bin,
        args,
        key_env,
        sign_in,
    }
}

/// Runs one ACP session until `commands` closes.
pub async fn run(
    cfg: SessionConfig,
    launch: Launch,
    mut commands: mpsc::Receiver<DriverCommand>,
    events: mpsc::Sender<AgentEvent>,
) {
    let mut agent: Option<Agent> = None;
    let mut incoming: Option<mpsc::Receiver<Incoming>> = None;
    let mut session_id = cfg.resume.clone();
    let mut prompt: Option<oneshot::Receiver<Result<Value, String>>> = None;
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut pending: HashMap<String, (Value, Vec<(String, String)>)> = HashMap::new();
    let mut turn = 0u64;
    let mut cancelling = false;
    let mut access = Access::default();
    let mut batch = Vec::new();
    let mut deadline = Instant::now();

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    DriverCommand::Prompt { text, settings, steer } => {
                        access = settings.access;
                        let text = format!("{}{text}", mode_prefix(settings.mode));
                        if prompt.is_some() {
                            // ACP v1 has no mid-turn input: steering cancels the turn and goes next.
                            if steer {
                                if let Some(agent) = &agent {
                                    agent.cancel();
                                    cancelling = true;
                                }
                                queue.push_front(text);
                            } else {
                                queue.push_back(text);
                            }
                            continue;
                        }
                        if agent.is_none() {
                            match Agent::start(&cfg, &launch, session_id.as_deref()).await {
                                Ok((started, rx)) => {
                                    session_id = Some(started.session.clone());
                                    let _ = events.send(AgentEvent::SessionStarted { session_id: started.session.clone(), model: String::new() }).await;
                                    agent = Some(started);
                                    incoming = Some(rx);
                                }
                                Err(e) => {
                                    fail(&events, e, StopReason::Error).await;
                                    continue;
                                }
                            }
                        }
                        turn += 1;
                        prompt = agent.as_ref().map(|agent| agent.prompt(&text));
                        deadline = Instant::now() + cfg.turn_timeout;
                    }
                    DriverCommand::Interrupt => {
                        if let (Some(agent), true) = (&agent, prompt.is_some()) {
                            agent.cancel();
                            cancelling = true;
                            queue.clear();
                        }
                    }
                    DriverCommand::Resolve { req_id, choice } => {
                        if let (Some(agent), Some((id, options))) = (&agent, pending.remove(&req_id)) {
                            agent.peer.respond(id, outcome(&options, choice));
                            deadline = Instant::now() + cfg.turn_timeout;
                        }
                    }
                    // ACP v1 asks no structured questions.
                    DriverCommand::Answer { .. } => {}
                }
            }
            msg = rpc::next(&mut incoming) => {
                let Some(msg) = msg else {
                    agent = None;
                    incoming = None;
                    pending.clear();
                    queue.clear();
                    if prompt.take().is_some() {
                        fail(&events, format!("{} exited mid-turn", cfg.bin.display()), StopReason::Error).await;
                    }
                    continue;
                };
                deadline = Instant::now() + if prompt.is_some() { cfg.turn_timeout } else { cfg.idle_timeout };
                match msg {
                    Incoming::Notification { method, params } if method == "session/update" => {
                        translate(&params["update"], &format!("acp-{turn}"), &cfg.blob_dir, &mut batch);
                    }
                    Incoming::Notification { .. } => {}
                    Incoming::Request { id, method, params } if method == "session/request_permission" => {
                        let options: Vec<(String, String)> = params["options"]
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_default()
                            .iter()
                            .map(|o| (string(&o["optionId"]), string(&o["kind"])))
                            .collect();
                        // Nobody is watching a task; Full access needs no one to.
                        let automatic = match (cfg.unattended, access) {
                            (true, _) => Some(PermChoice::Deny),
                            (false, Access::FullAccess) => Some(PermChoice::AllowOnce),
                            _ => None,
                        };
                        if let Some(choice) = automatic {
                            if let Some(agent) = &agent {
                                agent.peer.respond(id, outcome(&options, choice));
                            }
                            continue;
                        }
                        let call = &params["toolCall"];
                        let title = call["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("Allow this action?").to_owned();
                        let detail = if call["rawInput"].is_null() {
                            String::new()
                        } else {
                            format!("```json\n{}\n```", clip(&serde_json::to_string_pretty(&call["rawInput"]).unwrap_or_default(), 2000))
                        };
                        let req_id = id.to_string();
                        pending.insert(req_id.clone(), (id, options));
                        batch.push(AgentEvent::PermissionRequest { req_id, title, detail, plan: false });
                    }
                    Incoming::Request { id, .. } => {
                        if let Some(agent) = &agent {
                            agent.peer.respond_error(id, "Sorrel does not provide this capability");
                        }
                    }
                }
                for event in batch.drain(..) {
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
            }
            reply = wait(&mut prompt) => {
                prompt = None;
                pending.clear();
                let mut reason = match reply {
                    Ok(Ok(reply)) => match reply["stopReason"].as_str() {
                        Some("cancelled") => StopReason::Interrupted,
                        Some("max_tokens") | Some("max_turn_requests") => StopReason::MaxTokens,
                        Some("refusal") => {
                            let _ = events.send(AgentEvent::Error { message: "The agent refused to continue.".into() }).await;
                            StopReason::Error
                        }
                        _ => StopReason::EndTurn,
                    },
                    Ok(Err(e)) if !cancelling => {
                        let _ = events.send(AgentEvent::Error { message: e }).await;
                        StopReason::Error
                    }
                    _ => StopReason::Error,
                };
                if std::mem::take(&mut cancelling) {
                    reason = StopReason::Interrupted;
                }
                let _ = events.send(AgentEvent::TurnEnded { reason }).await;
                if let (Some(agent), Some(next)) = (&agent, queue.pop_front()) {
                    turn += 1;
                    prompt = Some(agent.prompt(&next));
                }
                deadline = Instant::now() + if prompt.is_some() { cfg.turn_timeout } else { cfg.idle_timeout };
            }
            // Paused while a card waits on the user.
            _ = sleep_until(deadline), if agent.is_some() && pending.is_empty() => {
                agent = None; // kill_on_drop
                incoming = None;
                queue.clear();
                if prompt.take().is_some() {
                    let message = format!("{} sent nothing for {}s, so it was stopped", cfg.bin.display(), cfg.turn_timeout.as_secs());
                    fail(&events, message, StopReason::Timeout).await;
                }
            }
        }
    }
}

struct Agent {
    peer: Peer,
    session: String,
    _child: Child,
    _stderr: StderrTail,
}

impl Agent {
    async fn start(
        cfg: &SessionConfig,
        launch: &Launch,
        resume: Option<&str>,
    ) -> Result<(Agent, mpsc::Receiver<Incoming>), String> {
        let mut cmd = crate::command(&cfg.bin);
        cmd.args(&launch.args).current_dir(&cfg.cwd);
        if let Some(key) = &cfg.api_key {
            cmd.env(launch.key_env, key);
        }
        let mut child = cmd.spawn().map_err(|e| crate::spawn_error(&cfg.bin, &e))?;
        let stderr = StderrTail::drain(child.stderr.take().expect("stderr is piped"));
        let (peer, mut incoming) = Peer::start(
            child.stdin.take().expect("stdin is piped"),
            child.stdout.take().expect("stdout is piped"),
        );
        let hint = |e: String| {
            if e.to_ascii_lowercase().contains("auth") {
                format!("{e}. {}", launch.sign_in)
            } else {
                e
            }
        };

        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
            "clientInfo": { "name": "sorrel", "title": "Sorrel", "version": env!("CARGO_PKG_VERSION") },
        });
        let init = peer
            .request("initialize", init, REQUEST_TIMEOUT)
            .await
            .map_err(hint)?;
        let cwd = cfg.cwd.to_string_lossy().into_owned();
        let mcp = mcp_servers(&cfg.mcp_servers);

        let mut session = None;
        if let Some(id) = resume
            && init["agentCapabilities"]["loadSession"] == true
        {
            let load = json!({ "sessionId": id, "cwd": cwd, "mcpServers": mcp });
            if peer
                .request("session/load", load, REQUEST_TIMEOUT)
                .await
                .is_ok()
            {
                // The replayed history arrived before the reply; the log already has it.
                while incoming.try_recv().is_ok() {}
                session = Some(id.to_owned());
            }
        }
        let session = match session {
            Some(session) => session,
            None => {
                let new = json!({ "cwd": cwd, "mcpServers": mcp });
                let reply = peer
                    .request("session/new", new, REQUEST_TIMEOUT)
                    .await
                    .map_err(hint)?;
                string(&reply["sessionId"])
            }
        };
        let agent = Agent {
            peer,
            session,
            _child: child,
            _stderr: stderr,
        };
        Ok((agent, incoming))
    }

    fn prompt(&self, text: &str) -> oneshot::Receiver<Result<Value, String>> {
        let prompt =
            json!({ "sessionId": self.session, "prompt": [{ "type": "text", "text": text }] });
        self.peer.call("session/prompt", prompt)
    }

    fn cancel(&self) {
        self.peer
            .notify("session/cancel", Some(json!({ "sessionId": self.session })));
    }
}

async fn wait(
    prompt: &mut Option<oneshot::Receiver<Result<Value, String>>>,
) -> Result<Result<Value, String>, oneshot::error::RecvError> {
    match prompt {
        Some(rx) => rx.await,
        None => std::future::pending().await,
    }
}

/// The reply to a permission request: the agent's option of the chosen kind.
fn outcome(options: &[(String, String)], choice: PermChoice) -> Value {
    let wanted: &[&str] = match choice {
        PermChoice::AllowOnce => &["allow_once", "allow_always"],
        PermChoice::AllowAlways => &["allow_always", "allow_once"],
        PermChoice::Deny => &["reject_once", "reject_always"],
    };
    let option = wanted
        .iter()
        .find_map(|kind| options.iter().find(|(_, k)| k == kind));
    match option {
        Some((id, _)) => json!({ "outcome": { "outcome": "selected", "optionId": id } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

fn mcp_servers(servers: &[McpServer]) -> Value {
    servers
        .iter()
        .map(|server| {
            let env: Vec<Value> = server
                .env
                .iter()
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect();
            json!({ "name": server.name, "command": server.command, "args": server.args, "env": env })
        })
        .collect()
}

/// Turns one `session/update` into [`AgentEvent`]s.
pub fn translate(update: &Value, msg_id: &str, blob_dir: &Path, out: &mut Vec<AgentEvent>) {
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => {
            if let Some(text) = update["content"]["text"].as_str() {
                out.push(AgentEvent::TextDelta {
                    msg_id: msg_id.to_owned(),
                    text: text.to_owned(),
                });
            }
        }
        Some("agent_thought_chunk") => {
            if let Some(text) = update["content"]["text"].as_str() {
                out.push(AgentEvent::ThinkingDelta {
                    msg_id: msg_id.to_owned(),
                    text: text.to_owned(),
                });
            }
        }
        Some("tool_call") => {
            out.push(AgentEvent::ToolCall {
                call_id: string(&update["toolCallId"]),
                kind: tool_kind(update["kind"].as_str().unwrap_or_default()),
                title: clip(&string(&update["title"]), 200),
            });
            tool_update(update, blob_dir, out);
        }
        Some("tool_call_update") => tool_update(update, blob_dir, out),
        Some("plan") => out.push(AgentEvent::Plan {
            items: update["entries"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|entry| TodoItem {
                    text: string(&entry["content"]),
                    status: match entry["status"].as_str() {
                        Some("completed") => TodoStatus::Done,
                        Some("in_progress") => TodoStatus::InProgress,
                        _ => TodoStatus::Pending,
                    },
                })
                .collect(),
        }),
        _ => {}
    }
}

fn tool_update(update: &Value, blob_dir: &Path, out: &mut Vec<AgentEvent>) {
    let status = match update["status"].as_str() {
        Some("completed") => ToolStatus::Done,
        Some("failed") => ToolStatus::Failed,
        _ => return,
    };
    let mut parts: Vec<String> = update["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|part| match part["type"].as_str() {
            Some("content") => part["content"]["text"].as_str().map(str::to_owned),
            Some("diff") => Some(format!("Edited {}", string(&part["path"]))),
            _ => None,
        })
        .collect();
    if parts.is_empty() && !update["rawOutput"].is_null() {
        parts.push(update["rawOutput"].to_string());
    }
    let (preview, output) = blob::store_output(blob_dir, &parts.join("\n"));
    out.push(AgentEvent::ToolUpdate {
        call_id: string(&update["toolCallId"]),
        status,
        preview,
        output,
    });
}

fn tool_kind(kind: &str) -> ToolKind {
    match kind {
        "read" => ToolKind::Read,
        "edit" | "delete" | "move" => ToolKind::Edit,
        "execute" => ToolKind::Execute,
        "search" => ToolKind::Search,
        "fetch" => ToolKind::Fetch,
        "think" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

fn string(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_become_events_and_permissions_pick_the_right_option() {
        let dir = std::env::temp_dir();
        let mut out = Vec::new();
        let updates = [
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Hi" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read a.rs", "kind": "read", "status": "pending" }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                    "content": [{ "type": "content", "content": { "type": "text", "text": "fn main() {}" } }] }),
            json!({ "sessionUpdate": "plan", "entries": [{ "content": "Step", "priority": "high", "status": "pending" }] }),
            json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] }),
        ];
        for update in &updates {
            translate(update, "acp-1", &dir, &mut out);
        }
        assert_eq!(out.len(), 4, "{out:#?}");
        assert!(
            matches!(&out[2], AgentEvent::ToolUpdate { preview, .. } if preview == "fn main() {}")
        );

        let options = vec![
            ("a".into(), "allow_once".into()),
            ("r".into(), "reject_once".into()),
        ];
        assert_eq!(
            outcome(&options, PermChoice::AllowAlways)["outcome"]["optionId"],
            "a"
        );
        assert_eq!(
            outcome(&options, PermChoice::Deny)["outcome"]["optionId"],
            "r"
        );
        assert_eq!(
            outcome(&[], PermChoice::Deny)["outcome"]["outcome"],
            "cancelled"
        );
    }
}

//! Codex driver. One shared `codex app-server` serves every thread over
//! JSON-RPC 2.0 on stdio.
//!
//! Wire shapes checked against the app-server protocol source
//! (`codex-rs/app-server-protocol`, v2) at codex-cli 0.160.0:
//! - `initialize` {clientInfo, capabilities.experimentalApi}, then `initialized`.
//! - `thread/start` {cwd, approvalPolicy, sandbox, developerInstructions, config}
//!   or `thread/resume` {threadId}.
//! - `turn/start` {threadId, input, approvalPolicy, sandboxPolicy}, `turn/steer`
//!   {threadId, input, expectedTurnId}, `turn/interrupt` {threadId, turnId}.
//! - Notifications: `item/agentMessage/delta`, `item/reasoning/*Delta`,
//!   `item/started|completed`, `turn/plan/updated`,
//!   `thread/tokenUsage/updated`, `turn/completed`, `error`.
//! - Server requests: `item/commandExecution/requestApproval` and
//!   `item/fileChange/requestApproval` (answered with {decision}),
//!   `item/tool/requestUserInput` (answered with {answers}).
//! - Sign-in: `account/read`, `account/login/start` {type: chatgpt | apiKey}.
//!   Codex runs the OAuth itself; the app never sees a token.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use proto::{
    Access, AgentEvent, AuthState, McpServer, Mode, ModelInfo, PermChoice, Question, StopReason,
    TodoItem, TodoStatus, ToolKind, ToolStatus, TurnSettings,
};
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};

use crate::{
    DriverCommand, SessionConfig, StderrTail, blob, clip, fail, mode_prefix,
    rpc::{self, Incoming, Peer},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type Routes = Arc<Mutex<HashMap<String, mpsc::Sender<Incoming>>>>;

/// The shared app-server. Cheap to clone. It starts on first use and again
/// after it dies; threads then resume by id.
#[derive(Clone)]
pub struct Server {
    bin: PathBuf,
    api_key: Option<String>,
    peer: Arc<tokio::sync::Mutex<Option<Peer>>>,
    routes: Routes,
    /// Notifications that belong to no thread, such as `account/login/completed`.
    notices: mpsc::Sender<(String, Value)>,
}

impl Server {
    pub fn new(
        bin: PathBuf,
        api_key: Option<String>,
        notices: mpsc::Sender<(String, Value)>,
    ) -> Self {
        Self {
            bin,
            api_key,
            peer: Arc::default(),
            routes: Arc::default(),
            notices,
        }
    }

    async fn peer(&self) -> Result<Peer, String> {
        let mut slot = self.peer.lock().await;
        if let Some(peer) = slot.as_ref() {
            return Ok(peer.clone());
        }
        let peer = self.start().await?;
        *slot = Some(peer.clone());
        Ok(peer)
    }

    async fn start(&self) -> Result<Peer, String> {
        let mut cmd = crate::command(&self.bin);
        cmd.arg("app-server");
        if let Some(key) = &self.api_key {
            cmd.env("OPENAI_API_KEY", key);
        }
        let mut child = cmd.spawn().map_err(|e| crate::spawn_error(&self.bin, &e))?;
        let stderr = StderrTail::drain(child.stderr.take().expect("stderr is piped"));
        let (peer, mut incoming) = Peer::start(
            child.stdin.take().expect("stdin is piped"),
            child.stdout.take().expect("stdout is piped"),
        );

        let routes = self.routes.clone();
        let notices = self.notices.clone();
        let slot = self.peer.clone();
        let responder = peer.clone();
        tokio::spawn(async move {
            let _child = child; // killed when this router ends
            let _stderr = stderr;
            while let Some(msg) = incoming.recv().await {
                let thread = match &msg {
                    Incoming::Notification { params, .. } | Incoming::Request { params, .. } => {
                        params["threadId"].as_str().map(str::to_owned)
                    }
                };
                let route = thread.and_then(|id| routes.lock().unwrap().get(&id).cloned());
                match (route, msg) {
                    // ponytail: a thread that stops draining stalls the router; channels are deep enough for a turn.
                    (Some(route), msg) => {
                        let _ = route.send(msg).await;
                    }
                    (None, Incoming::Notification { method, params }) => {
                        let _ = notices.try_send((method, params));
                    }
                    (None, Incoming::Request { id, .. }) => {
                        responder.respond_error(id, "no open thread for this request");
                    }
                }
            }
            // The process is gone: closing every route tells each thread.
            routes.lock().unwrap().clear();
            *slot.lock().await = None;
        });

        let client = json!({
            "clientInfo": { "name": "sorrel", "title": "Sorrel", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        });
        peer.request("initialize", client, REQUEST_TIMEOUT).await?;
        peer.notify("initialized", None);
        if let Some(key) = &self.api_key {
            let login = json!({ "type": "apiKey", "apiKey": key });
            peer.request("account/login/start", login, REQUEST_TIMEOUT)
                .await?;
        }
        Ok(peer)
    }

    fn route(&self, thread: &str) -> mpsc::Receiver<Incoming> {
        let (tx, rx) = mpsc::channel(1024);
        self.routes.lock().unwrap().insert(thread.to_owned(), tx);
        rx
    }

    fn unroute(&self, thread: &str) {
        self.routes.lock().unwrap().remove(thread);
    }

    /// Asks the app-server who is signed in.
    pub async fn auth(&self) -> (AuthState, String) {
        let peer = match self.peer().await {
            Ok(peer) => peer,
            // spawn_error says "was not found" when the binary is missing.
            Err(e) if e.contains("was not found") => return (AuthState::Missing, e),
            Err(e) => return (AuthState::Unknown, e),
        };
        match peer
            .request(
                "account/read",
                json!({ "refreshToken": false }),
                REQUEST_TIMEOUT,
            )
            .await
        {
            Ok(reply) => match reply["account"]["type"].as_str() {
                Some("chatgpt") => (
                    AuthState::Subscription,
                    format!(
                        "ChatGPT {}",
                        reply["account"]["email"].as_str().unwrap_or_default()
                    ),
                ),
                Some("apiKey") => (AuthState::ApiKey, "OpenAI API key".into()),
                Some(other) => (AuthState::Subscription, other.to_owned()),
                None if reply["requiresOpenaiAuth"] == false => (
                    AuthState::Unknown,
                    "No sign-in needed for this provider".into(),
                ),
                None => (AuthState::SignedOut, "Sign in with ChatGPT".into()),
            },
            Err(e) => (AuthState::Unknown, e),
        }
    }

    /// The models this Codex offers, from `model/list`.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, String> {
        let peer = self.peer().await?;
        let reply = peer
            .request(
                "model/list",
                json!({ "includeHidden": false }),
                REQUEST_TIMEOUT,
            )
            .await?;
        Ok(reply["data"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|model| model["hidden"] != true)
            .map(|model| ModelInfo {
                id: string(&model["model"]),
                label: model["displayName"]
                    .as_str()
                    .or(model["model"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                description: string(&model["description"]),
                efforts: model["supportedReasoningEfforts"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|option| option["reasoningEffort"].as_str().map(str::to_owned))
                    .collect(),
            })
            .collect())
    }

    /// Starts Codex's own ChatGPT sign-in and returns the page to open.
    pub async fn sign_in(&self) -> Result<String, String> {
        let peer = self.peer().await?;
        let reply = peer
            .request(
                "account/login/start",
                json!({ "type": "chatgpt" }),
                REQUEST_TIMEOUT,
            )
            .await?;
        reply["authUrl"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Codex did not return a sign-in link".into())
    }
}

/// Asks `codex login status`, which checks sign-in without starting the
/// app-server. It reports on stderr; the key itself is never echoed back.
pub async fn login_status(bin: &std::path::Path) -> (AuthState, String) {
    let mut cmd = crate::command(bin);
    cmd.args(["login", "status"])
        .stdin(std::process::Stdio::null());
    let output = match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Err(_) => return (AuthState::Unknown, "`codex login status` timed out".into()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return (AuthState::Missing, crate::spawn_error(bin, &e));
        }
        Ok(Err(e)) => return (AuthState::Unknown, e.to_string()),
        Ok(Ok(output)) => output,
    };
    let text = String::from_utf8_lossy(&output.stderr).to_string()
        + &String::from_utf8_lossy(&output.stdout);
    match (
        output.status.success(),
        text.contains("ChatGPT"),
        text.contains("API key"),
    ) {
        (true, true, _) => (AuthState::Subscription, "ChatGPT".into()),
        (true, _, true) => (AuthState::ApiKey, "OpenAI API key".into()),
        (true, _, _) => (AuthState::Subscription, "Signed in".into()),
        (false, ..) => (AuthState::SignedOut, "Sign in with ChatGPT".into()),
    }
}

/// Runs one Codex thread until `commands` closes.
pub async fn run(
    server: Server,
    cfg: SessionConfig,
    mut commands: mpsc::Receiver<DriverCommand>,
    events: mpsc::Sender<AgentEvent>,
) {
    let mut thread = cfg.resume.clone();
    let mut incoming: Option<mpsc::Receiver<Incoming>> = None;
    let mut turn: Option<String> = None;
    let mut pending: HashMap<String, (Value, Pending)> = HashMap::new();
    let mut batch = Vec::new();
    let mut deadline = Instant::now();

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                let peer = match server.peer().await {
                    Ok(peer) => peer,
                    Err(e) => {
                        if matches!(command, DriverCommand::Prompt { .. }) {
                            fail(&events, e, StopReason::Error).await;
                        }
                        continue;
                    }
                };
                match command {
                    DriverCommand::Prompt { text, settings, steer } => {
                        if incoming.is_none() {
                            match open(&server, &peer, &cfg, thread.as_deref()).await {
                                Ok((id, rx, model)) => {
                                    let _ = events.send(AgentEvent::SessionStarted { session_id: id.clone(), model }).await;
                                    thread = Some(id);
                                    incoming = Some(rx);
                                }
                                Err(e) => {
                                    fail(&events, e, StopReason::Error).await;
                                    continue;
                                }
                            }
                        }
                        let id = thread.clone().expect("opened above");
                        let input = json!([{ "type": "text", "text": format!("{}{text}", mode_prefix(settings.mode)) }]);
                        if let (true, Some(turn_id)) = (steer, turn.as_ref()) {
                            let steer = json!({ "threadId": id, "input": input, "expectedTurnId": turn_id });
                            if let Err(e) = peer.request("turn/steer", steer, REQUEST_TIMEOUT).await {
                                let _ = events.send(AgentEvent::Error { message: format!("Could not steer the turn: {e}") }).await;
                            }
                            continue;
                        }
                        let (approval, sandbox) = policy(&settings, cfg.unattended);
                        let mut start = json!({ "threadId": id, "input": input, "approvalPolicy": approval, "sandboxPolicy": sandbox });
                        if let Some(model) = &settings.model {
                            start["model"] = json!(model);
                        }
                        if let Some(effort) = &settings.effort {
                            start["effort"] = json!(effort);
                        }
                        match peer.request("turn/start", start, REQUEST_TIMEOUT).await {
                            Ok(started) => {
                                turn = Some(string(&started["turn"]["id"]));
                                deadline = Instant::now() + cfg.turn_timeout;
                            }
                            Err(e) => fail(&events, e, StopReason::Error).await,
                        }
                    }
                    DriverCommand::Interrupt => {
                        if let (Some(id), Some(turn_id)) = (&thread, &turn) {
                            let interrupt = json!({ "threadId": id, "turnId": turn_id });
                            let _ = peer.request("turn/interrupt", interrupt, REQUEST_TIMEOUT).await;
                        }
                    }
                    DriverCommand::Resolve { req_id, choice } => {
                        if let Some((rpc_id, request)) = pending.remove(&req_id) {
                            peer.respond(rpc_id, request.decision(choice));
                            deadline = Instant::now() + cfg.turn_timeout;
                        }
                    }
                    DriverCommand::Answer { req_id, answers } => {
                        if let Some((rpc_id, request)) = pending.remove(&req_id) {
                            peer.respond(rpc_id, request.answer(&answers));
                            deadline = Instant::now() + cfg.turn_timeout;
                        }
                    }
                }
            }
            msg = rpc::next(&mut incoming) => {
                let Some(msg) = msg else {
                    // The app-server died. The next prompt restarts it and resumes this thread.
                    incoming = None;
                    pending.clear();
                    if turn.take().is_some() {
                        fail(&events, "Codex stopped in the middle of the turn".into(), StopReason::Error).await;
                    }
                    continue;
                };
                deadline = Instant::now() + cfg.turn_timeout;
                match msg {
                    Incoming::Notification { method, params } => {
                        translate(&method, &params, &cfg.blob_dir, &mut batch);
                    }
                    Incoming::Request { id, method, params } => match card(&method, &params, &id) {
                        Some((_, request)) if cfg.unattended => {
                            if let Ok(peer) = server.peer().await {
                                peer.respond(id, request.refusal());
                            }
                        }
                        Some((event, request)) => {
                            pending.insert(id.to_string(), (id, request));
                            batch.push(event);
                        }
                        None => {
                            if let Ok(peer) = server.peer().await {
                                peer.respond_error(id, "Sorrel does not handle this request");
                            }
                        }
                    },
                }
                for event in batch.drain(..) {
                    if matches!(event, AgentEvent::TurnEnded { .. }) {
                        turn = None;
                        pending.clear();
                    }
                    if events.send(event).await.is_err() {
                        if let Some(id) = &thread {
                            server.unroute(id);
                        }
                        return;
                    }
                }
            }
            // Paused while a card waits on the user.
            _ = sleep_until(deadline), if turn.is_some() && pending.is_empty() => {
                if let (Some(id), Some(turn_id)) = (&thread, turn.take())
                    && let Ok(peer) = server.peer().await
                {
                    let interrupt = json!({ "threadId": id, "turnId": turn_id });
                    let _ = peer.request("turn/interrupt", interrupt, REQUEST_TIMEOUT).await;
                }
                let message = format!("Codex sent nothing for {}s, so the turn was stopped", cfg.turn_timeout.as_secs());
                fail(&events, message, StopReason::Timeout).await;
            }
        }
    }
    if let Some(id) = &thread {
        server.unroute(id);
    }
}

/// Resumes `resume` if Codex still has it, else starts a new thread.
async fn open(
    server: &Server,
    peer: &Peer,
    cfg: &SessionConfig,
    resume: Option<&str>,
) -> Result<(String, mpsc::Receiver<Incoming>, String), String> {
    let cwd = cfg.cwd.to_string_lossy().into_owned();
    let mut reply = None;
    if let Some(id) = resume {
        let params = json!({ "threadId": id, "cwd": cwd });
        reply = peer
            .request("thread/resume", params, REQUEST_TIMEOUT)
            .await
            .ok();
    }
    let reply = match reply {
        Some(reply) => reply,
        None => {
            let mut params = json!({
                "cwd": cwd,
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write",
                "config": mcp_config(&cfg.mcp_servers),
            });
            if !cfg.instructions.trim().is_empty() {
                params["developerInstructions"] = json!(cfg.instructions);
            }
            peer.request("thread/start", params, REQUEST_TIMEOUT)
                .await?
        }
    };
    let id = string(&reply["thread"]["id"]);
    if id.is_empty() {
        return Err("Codex did not return a thread id".into());
    }
    let rx = server.route(&id);
    Ok((id, rx, string(&reply["model"])))
}

/// Approval policy and sandbox for a turn.
fn policy(settings: &TurnSettings, unattended: bool) -> (&'static str, Value) {
    let write = json!({ "type": "workspaceWrite", "networkAccess": false });
    let read = json!({ "type": "readOnly" });
    let full = json!({ "type": "dangerFullAccess" });
    match (unattended, settings.mode, settings.access) {
        (true, ..) => ("never", write),
        (false, Mode::Plan | Mode::Ask, _) => ("on-request", read),
        // Codex applies edits inside the workspace without asking in this sandbox.
        (false, Mode::Agent, Access::Supervised | Access::AutoEdits) => ("on-request", write),
        (false, Mode::Agent, Access::FullAccess) => ("never", full),
    }
}

/// MCP servers as dotted config overrides on top of the user's `mcp_servers`.
fn mcp_config(servers: &[McpServer]) -> Value {
    let mut config = serde_json::Map::new();
    for server in servers {
        let key = |field: &str| format!("mcp_servers.{}.{field}", server.name);
        config.insert(key("command"), json!(server.command));
        config.insert(key("args"), json!(server.args));
        let env: serde_json::Map<String, Value> = server
            .env
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        config.insert(key("env"), Value::Object(env));
    }
    Value::Object(config)
}

enum Pending {
    Approval,
    Questions(Vec<Question>),
}

impl Pending {
    fn decision(&self, choice: PermChoice) -> Value {
        let decision = match choice {
            PermChoice::AllowOnce => "accept",
            PermChoice::AllowAlways => "acceptForSession",
            PermChoice::Deny => "decline",
        };
        json!({ "decision": decision })
    }

    fn answer(&self, answers: &[Vec<String>]) -> Value {
        let Pending::Questions(questions) = self else {
            return self.decision(PermChoice::Deny);
        };
        let by_id: serde_json::Map<String, Value> = questions
            .iter()
            .enumerate()
            .map(|(ix, q)| {
                let labels = answers.get(ix).cloned().unwrap_or_default();
                (q.id.clone(), json!({ "answers": labels }))
            })
            .collect();
        json!({ "answers": by_id })
    }

    /// The reply when nobody is there to ask.
    fn refusal(&self) -> Value {
        match self {
            Pending::Approval => self.decision(PermChoice::Deny),
            Pending::Questions(_) => json!({ "answers": {} }),
        }
    }
}

/// The card for a server request, if it is one Sorrel shows.
fn card(method: &str, params: &Value, id: &Value) -> Option<(AgentEvent, Pending)> {
    let req_id = id.to_string();
    let reason = string(&params["reason"]);
    match method {
        "item/commandExecution/requestApproval" => {
            let command = format!("```sh\n{}\n```", string(&params["command"]));
            Some((
                AgentEvent::PermissionRequest {
                    req_id,
                    title: "Run a command".into(),
                    detail: if reason.is_empty() {
                        command
                    } else {
                        format!("{reason}\n\n{command}")
                    },
                    plan: false,
                },
                Pending::Approval,
            ))
        }
        "item/fileChange/requestApproval" => Some((
            AgentEvent::PermissionRequest {
                req_id,
                title: "Apply file changes".into(),
                detail: reason,
                plan: false,
            },
            Pending::Approval,
        )),
        "item/tool/requestUserInput" => {
            let questions: Vec<Question> = params["questions"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|q| Question {
                    id: string(&q["id"]),
                    header: string(&q["header"]),
                    text: string(&q["question"]),
                    options: q["options"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .map(|option| string(&option["label"]))
                        .collect(),
                    multi: false,
                })
                .collect();
            Some((
                AgentEvent::Question {
                    req_id,
                    questions: questions.clone(),
                },
                Pending::Questions(questions),
            ))
        }
        _ => None,
    }
}

/// Turns one app-server notification into [`AgentEvent`]s.
pub fn translate(
    method: &str,
    params: &Value,
    blob_dir: &std::path::Path,
    out: &mut Vec<AgentEvent>,
) {
    match method {
        "item/agentMessage/delta" => out.push(AgentEvent::TextDelta {
            msg_id: string(&params["itemId"]),
            text: string(&params["delta"]),
        }),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            out.push(AgentEvent::ThinkingDelta {
                msg_id: string(&params["itemId"]),
                text: string(&params["delta"]),
            })
        }
        "item/started" => {
            let item = &params["item"];
            if let Some((kind, title)) = tool(item) {
                out.push(AgentEvent::ToolCall {
                    call_id: string(&item["id"]),
                    kind,
                    title,
                });
            }
        }
        "item/completed" => {
            let item = &params["item"];
            if tool(item).is_some() {
                let (preview, output) = blob::store_output(blob_dir, &tool_output(item));
                out.push(AgentEvent::ToolUpdate {
                    call_id: string(&item["id"]),
                    status: if item["status"] == "completed" {
                        ToolStatus::Done
                    } else {
                        ToolStatus::Failed
                    },
                    preview,
                    output,
                });
            }
        }
        "turn/plan/updated" => out.push(AgentEvent::Plan {
            items: params["plan"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|step| TodoItem {
                    text: string(&step["step"]),
                    status: match step["status"].as_str() {
                        Some("completed") => TodoStatus::Done,
                        Some("inProgress") => TodoStatus::InProgress,
                        _ => TodoStatus::Pending,
                    },
                })
                .collect(),
        }),
        "thread/tokenUsage/updated" => {
            let last = &params["tokenUsage"]["last"];
            out.push(AgentEvent::Usage {
                input: last["inputTokens"].as_u64().unwrap_or(0),
                output: last["outputTokens"].as_u64().unwrap_or(0),
            });
        }
        "turn/completed" => {
            let turn = &params["turn"];
            let reason = match turn["status"].as_str() {
                Some("failed") => {
                    let message = turn["error"]["message"]
                        .as_str()
                        .unwrap_or("The Codex turn failed");
                    out.push(AgentEvent::Error {
                        message: message.to_owned(),
                    });
                    StopReason::Error
                }
                Some("interrupted") => StopReason::Interrupted,
                _ => StopReason::EndTurn,
            };
            out.push(AgentEvent::TurnEnded { reason });
        }
        "error" if params["willRetry"] != true => out.push(AgentEvent::Error {
            message: string(&params["error"]["message"]),
        }),
        _ => {}
    }
}

fn tool(item: &Value) -> Option<(ToolKind, String)> {
    let title = match item["type"].as_str()? {
        "commandExecution" => (
            ToolKind::Execute,
            format!("Run: {}", string(&item["command"])),
        ),
        "fileChange" => {
            let paths: Vec<&str> = item["changes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|change| change["path"].as_str())
                .collect();
            (ToolKind::Edit, format!("Edit {}", paths.join(", ")))
        }
        "mcpToolCall" => (
            ToolKind::Other,
            format!("{}: {}", string(&item["server"]), string(&item["tool"])),
        ),
        "dynamicToolCall" => (ToolKind::Other, string(&item["tool"])),
        "webSearch" => (
            ToolKind::Fetch,
            format!("Search: {}", string(&item["query"])),
        ),
        _ => return None,
    };
    Some((title.0, clip(&title.1, 200)))
}

fn tool_output(item: &Value) -> String {
    match item["type"].as_str() {
        Some("commandExecution") => string(&item["aggregatedOutput"]),
        Some("fileChange") => item["changes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|change| format!("{}\n{}", string(&change["path"]), string(&change["diff"])))
            .collect::<Vec<_>>()
            .join("\n"),
        Some("mcpToolCall") => {
            let parts: Vec<&str> = item["result"]["content"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect();
            if parts.is_empty() {
                string(&item["error"]["message"])
            } else {
                parts.join("\n")
            }
        }
        _ => String::new(),
    }
}

fn string(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_become_events() {
        let dir = std::env::temp_dir();
        let mut out = Vec::new();
        let notes = [
            (
                "item/agentMessage/delta",
                json!({ "threadId": "t", "turnId": "u", "itemId": "i1", "delta": "Hi" }),
            ),
            (
                "item/started",
                json!({ "item": { "type": "commandExecution", "id": "c1", "command": "ls", "status": "inProgress" } }),
            ),
            (
                "item/completed",
                json!({ "item": { "type": "commandExecution", "id": "c1", "command": "ls", "status": "completed", "aggregatedOutput": "a\nb" } }),
            ),
            (
                "turn/plan/updated",
                json!({ "plan": [{ "step": "one", "status": "inProgress" }] }),
            ),
            (
                "thread/tokenUsage/updated",
                json!({ "tokenUsage": { "last": { "inputTokens": 5, "outputTokens": 7 } } }),
            ),
            (
                "item/started",
                json!({ "item": { "type": "agentMessage", "id": "i1", "text": "" } }),
            ),
            (
                "turn/completed",
                json!({ "turn": { "id": "u", "status": "failed", "error": { "message": "boom" } } }),
            ),
        ];
        for (method, params) in &notes {
            translate(method, params, &dir, &mut out);
        }
        assert_eq!(
            out,
            vec![
                AgentEvent::TextDelta {
                    msg_id: "i1".into(),
                    text: "Hi".into()
                },
                AgentEvent::ToolCall {
                    call_id: "c1".into(),
                    kind: ToolKind::Execute,
                    title: "Run: ls".into()
                },
                AgentEvent::ToolUpdate {
                    call_id: "c1".into(),
                    status: ToolStatus::Done,
                    preview: "a\nb".into(),
                    output: None
                },
                AgentEvent::Plan {
                    items: vec![TodoItem {
                        text: "one".into(),
                        status: TodoStatus::InProgress
                    }]
                },
                AgentEvent::Usage {
                    input: 5,
                    output: 7
                },
                AgentEvent::Error {
                    message: "boom".into()
                },
                AgentEvent::TurnEnded {
                    reason: StopReason::Error
                },
            ]
        );
    }

    #[test]
    fn approvals_round_trip() {
        let (event, pending) = card(
            "item/commandExecution/requestApproval",
            &json!({ "command": "rm -rf build" }),
            &json!(7),
        )
        .unwrap();
        assert!(matches!(event, AgentEvent::PermissionRequest { ref req_id, .. } if req_id == "7"));
        assert_eq!(
            pending.decision(PermChoice::AllowAlways),
            json!({ "decision": "acceptForSession" })
        );
        assert_eq!(pending.refusal(), json!({ "decision": "decline" }));
    }
}

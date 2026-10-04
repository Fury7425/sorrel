//! Claude Code driver.
//!
//! Spawns the unmodified `claude` binary in stream-json mode, writes user
//! messages to its stdin and translates its stdout into [`AgentEvent`]s.
//!
//! - Flags checked against `claude --help` (2.1.286). Permission prompts ride
//!   the stdio control channel (`--permission-prompt-tool stdio`, the channel
//!   the Agent SDK uses; undocumented, validated live by Zeron on 2.1.228 and
//!   2.1.280): `can_use_tool` control requests arrive on stdout and are
//!   answered with `control_response` lines on stdin.
//! - `AskUserQuestion` and `ExitPlanMode` arrive as `can_use_tool` too and
//!   become question and plan-approval cards.
//! - Steering is another user line with `"priority": "now"`; interrupts and
//!   mode switches are client control requests.
//! - Subagent traffic carries a `parent_tool_use_id` and stays out of the
//!   main transcript.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use proto::{
    Access, AgentEvent, AuthState, McpServer, Mode, ModelInfo, MsgId, PermChoice, Question,
    StopReason, TodoItem, TodoStatus, ToolKind, ToolStatus, TurnSettings,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc,
    time::{Instant, sleep_until},
};

use crate::{DriverCommand, SessionConfig, StderrTail, blob, clip, fail};

/// Tools Ask mode refuses without asking.
const WRITE_TOOLS: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];

/// The models the picker offers. The CLI has no list command, so these are
/// its documented aliases plus a pinned small model.
pub fn models() -> Vec<ModelInfo> {
    let efforts: Vec<String> = ["low", "medium", "high", "xhigh", "max"]
        .map(String::from)
        .to_vec();
    [
        ("fable", "Fable", "Most capable"),
        ("opus", "Opus", "Deep reasoning for hard work"),
        ("sonnet", "Sonnet", "Fast and capable"),
        (
            "claude-haiku-4-5-20251001",
            "Haiku 4.5",
            "Quickest, for small tasks",
        ),
    ]
    .into_iter()
    .map(|(id, label, description)| ModelInfo {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        efforts: efforts.clone(),
    })
    .collect()
}

/// Asks `claude auth status`. Never reads credential files.
pub async fn auth_status(bin: &Path) -> (AuthState, String) {
    let mut cmd = crate::command(bin);
    cmd.args(["auth", "status"]).stdin(Stdio::null());
    let output = match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Err(_) => return (AuthState::Unknown, "`claude auth status` timed out".into()),
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
            return (AuthState::Missing, crate::spawn_error(bin, &e));
        }
        Ok(Err(e)) => return (AuthState::Unknown, e.to_string()),
        Ok(Ok(output)) => output,
    };
    let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let method = status["authMethod"].as_str().unwrap_or_default().to_owned();
    match status["loggedIn"].as_bool() {
        Some(true) if method.to_ascii_lowercase().contains("key") => (AuthState::ApiKey, method),
        Some(true) => (AuthState::Subscription, method),
        Some(false) => (
            AuthState::SignedOut,
            "Run `claude auth login` in a terminal.".into(),
        ),
        None => (
            AuthState::Unknown,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
    }
}

/// The check before every new process: a missing or signed-out CLI fails the
/// turn with a message instead of a confusing stream error.
async fn preflight(cfg: &SessionConfig) -> Result<(), String> {
    if cfg.api_key.is_some() {
        return Ok(());
    }
    match auth_status(&cfg.bin).await {
        (AuthState::Missing, detail) => Err(detail),
        (AuthState::SignedOut, _) => Err(
            "Claude Code is not signed in. Run `claude auth login` in a terminal, or add an Anthropic API key in Settings."
                .into(),
        ),
        _ => Ok(()),
    }
}

/// Runs one Claude session until `commands` closes.
pub async fn run(
    cfg: SessionConfig,
    mut commands: mpsc::Receiver<DriverCommand>,
    events: mpsc::Sender<AgentEvent>,
) {
    let mut process: Option<Process> = None;
    let mut session_id = cfg.resume.clone();
    let mut translator = Translator::new(cfg.blob_dir.clone());
    let mut batch = Vec::new();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut current = TurnSettings::default();
    let mut in_turn = false;
    let mut interrupting = false;
    let mut started = false;
    let mut next_request = 0u64;
    let mut deadline = Instant::now();

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    DriverCommand::Prompt { text, settings: wanted, steer } => {
                        // The effort is fixed when claude starts: switch it between turns by restarting.
                        if process.is_some() && !in_turn && wanted.effort != current.effort {
                            process = None;
                        }
                        if process.is_none() {
                            if let Err(message) = preflight(&cfg).await {
                                fail(&events, message, StopReason::Error).await;
                                continue;
                            }
                            match Process::spawn(&cfg, session_id.as_deref(), &wanted) {
                                Ok(spawned) => {
                                    process = Some(spawned);
                                    current = wanted.clone();
                                    started = false;
                                }
                                Err(message) => {
                                    fail(&events, message, StopReason::Error).await;
                                    continue;
                                }
                            }
                        }
                        let running = process.as_mut().expect("spawned above");
                        let mut written = Ok(());
                        if permission_mode(&wanted) != permission_mode(&current) {
                            next_request += 1;
                            let request = json!({ "subtype": "set_permission_mode", "mode": permission_mode(&wanted) });
                            written = running.write(&control_request(next_request, request)).await;
                        }
                        if written.is_ok() && wanted.model != current.model {
                            next_request += 1;
                            let request = json!({ "subtype": "set_model", "model": wanted.model });
                            written = running.write(&control_request(next_request, request)).await;
                        }
                        current = TurnSettings { effort: current.effort.clone(), ..wanted };
                        if written.is_ok() {
                            written = running.write(&user_line(&text, steer && in_turn)).await;
                        }
                        if let Err(e) = written {
                            let message = format!("claude stopped reading input: {e}\n{}", running.stderr.text());
                            process = None;
                            pending.clear();
                            in_turn = false;
                            fail(&events, message, StopReason::Error).await;
                            continue;
                        }
                        in_turn = true;
                        deadline = Instant::now() + cfg.turn_timeout;
                    }
                    DriverCommand::Interrupt => {
                        if let (true, Some(running)) = (in_turn, process.as_mut()) {
                            next_request += 1;
                            interrupting = true;
                            let _ = running.write(&control_request(next_request, json!({ "subtype": "interrupt" }))).await;
                        }
                    }
                    DriverCommand::Resolve { req_id, choice } => {
                        if let (Some(running), Some(request)) = (process.as_mut(), pending.remove(&req_id)) {
                            if matches!(request.kind, PendingKind::Plan) && choice != PermChoice::Deny {
                                // Claude leaves plan mode once its plan is approved.
                                current.mode = Mode::Agent;
                            }
                            let _ = running.write(&control_response(&req_id, request.response(choice))).await;
                            deadline = Instant::now() + cfg.turn_timeout;
                        }
                    }
                    DriverCommand::Answer { req_id, answers } => {
                        if let (Some(running), Some(request)) = (process.as_mut(), pending.remove(&req_id)) {
                            let _ = running.write(&control_response(&req_id, request.answer(&answers))).await;
                            deadline = Instant::now() + cfg.turn_timeout;
                        }
                    }
                }
            }
            line = next_line(&mut process) => {
                let Some(line) = line else {
                    // stdout closed, so the process is gone.
                    let tail = process.take().map(|p| p.stderr.text()).unwrap_or_default();
                    pending.clear();
                    if !started {
                        // A resume that dies before `init` names a session claude no longer has.
                        session_id = None;
                    }
                    if std::mem::take(&mut in_turn) {
                        interrupting = false;
                        fail(&events, format!("claude exited mid-turn\n{tail}"), StopReason::Error).await;
                    }
                    continue;
                };
                if let Some(request) = translator.translate(&line, &mut batch) {
                    match decide(&request, current.mode, cfg.unattended) {
                        Decision::Reply(response) => {
                            if let Some(running) = process.as_mut() {
                                let _ = running.write(&control_response(&request.request_id, response)).await;
                            }
                        }
                        Decision::Ask(event, entry) => {
                            pending.insert(request.request_id.clone(), entry);
                            batch.push(event);
                        }
                    }
                }
                for event in batch.drain(..) {
                    let event = match event {
                        AgentEvent::SessionStarted { session_id: ref id, .. } => {
                            session_id = Some(id.clone());
                            started = true;
                            event
                        }
                        AgentEvent::Error { .. } if interrupting => continue,
                        AgentEvent::TurnEnded { .. } => {
                            in_turn = false;
                            pending.clear();
                            if std::mem::take(&mut interrupting) {
                                AgentEvent::TurnEnded { reason: StopReason::Interrupted }
                            } else {
                                event
                            }
                        }
                        other => other,
                    };
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
                deadline = Instant::now() + if in_turn { cfg.turn_timeout } else { cfg.idle_timeout };
            }
            // Paused while a card waits on the user.
            _ = sleep_until(deadline), if process.is_some() && pending.is_empty() => {
                process = None; // kill_on_drop
                if std::mem::take(&mut in_turn) {
                    interrupting = false;
                    let message = format!("claude printed nothing for {}s, so it was stopped", cfg.turn_timeout.as_secs());
                    fail(&events, message, StopReason::Timeout).await;
                }
            }
        }
    }
}

/// Streams a recorded stdout capture through the translator as if `claude`
/// were running, sleeping `pace` after each text delta.
pub async fn replay(
    fixture: String,
    pace: Duration,
    blob_dir: PathBuf,
    events: mpsc::Sender<AgentEvent>,
) {
    let mut translator = Translator::new(blob_dir);
    let mut batch = Vec::new();
    for line in fixture.lines() {
        translator.translate(line, &mut batch);
        for event in batch.drain(..) {
            let delta = matches!(
                event,
                AgentEvent::TextDelta { .. } | AgentEvent::ThinkingDelta { .. }
            );
            if events.send(event).await.is_err() {
                return;
            }
            if delta {
                tokio::time::sleep(pace).await;
            }
        }
    }
}

/// A `can_use_tool` request from the CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlRequest {
    pub request_id: String,
    pub tool_name: String,
    pub input: Value,
    pub suggestions: Value,
    pub description: String,
}

/// Turns stream-json stdout lines into [`AgentEvent`]s. Anything it does not
/// recognize, including malformed lines, is dropped.
pub struct Translator {
    msg_id: MsgId,
    blob_dir: PathBuf,
}

impl Translator {
    pub fn new(blob_dir: PathBuf) -> Self {
        Self {
            msg_id: MsgId::new(),
            blob_dir,
        }
    }

    /// Appends the events in `line` to `out`; returns a permission request the
    /// caller must answer.
    pub fn translate(&mut self, line: &str, out: &mut Vec<AgentEvent>) -> Option<ControlRequest> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return None;
        };
        // Subagent traffic carries a parent tool id; it is not the main transcript.
        let main = v["parent_tool_use_id"].is_null();
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => out.push(AgentEvent::SessionStarted {
                session_id: string(&v["session_id"]),
                model: string(&v["model"]),
            }),
            Some("stream_event") if main => {
                let event = &v["event"];
                match event["type"].as_str() {
                    Some("message_start") => self.msg_id = string(&event["message"]["id"]),
                    Some("content_block_delta") => {
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => out.push(AgentEvent::TextDelta {
                                msg_id: self.msg_id.clone(),
                                text: string(&delta["text"]),
                            }),
                            Some("thinking_delta") => out.push(AgentEvent::ThinkingDelta {
                                msg_id: self.msg_id.clone(),
                                text: string(&delta["thinking"]),
                            }),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") if main => {
                for block in blocks(&v["message"]["content"]) {
                    if block["type"] != "tool_use" {
                        continue;
                    }
                    let name = block["name"].as_str().unwrap_or_default();
                    match name {
                        "TodoWrite" => out.push(AgentEvent::Plan {
                            items: todos(&block["input"]),
                        }),
                        // These become question and plan cards instead.
                        "AskUserQuestion" | "ExitPlanMode" => {}
                        _ => out.push(AgentEvent::ToolCall {
                            call_id: string(&block["id"]),
                            kind: tool_kind(name),
                            title: tool_title(name, &block["input"]),
                        }),
                    }
                }
            }
            Some("user") if main => {
                for block in blocks(&v["message"]["content"]) {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let text = match &block["content"] {
                        Value::String(text) => text.clone(),
                        Value::Array(parts) => parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    let (preview, output) = blob::store_output(&self.blob_dir, &text);
                    out.push(AgentEvent::ToolUpdate {
                        call_id: string(&block["tool_use_id"]),
                        status: if block["is_error"] == true {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Done
                        },
                        preview,
                        output,
                    });
                }
            }
            Some("control_request") if v["request"]["subtype"] == "can_use_tool" => {
                let request = &v["request"];
                return Some(ControlRequest {
                    request_id: string(&v["request_id"]),
                    tool_name: string(&request["tool_name"]),
                    input: request["input"].clone(),
                    suggestions: request["permission_suggestions"].clone(),
                    description: string(&request["description"]),
                });
            }
            Some("result") => {
                let usage = &v["usage"];
                let tokens = |key: &str| usage[key].as_u64().unwrap_or(0);
                out.push(AgentEvent::Usage {
                    input: tokens("input_tokens")
                        + tokens("cache_creation_input_tokens")
                        + tokens("cache_read_input_tokens"),
                    output: tokens("output_tokens"),
                });
                let reason = if v["is_error"] == true {
                    let message = v["result"]
                        .as_str()
                        .or(v["subtype"].as_str())
                        .unwrap_or("error");
                    out.push(AgentEvent::Error {
                        message: message.to_owned(),
                    });
                    StopReason::Error
                } else if v["stop_reason"] == "max_tokens" {
                    StopReason::MaxTokens
                } else {
                    StopReason::EndTurn
                };
                out.push(AgentEvent::TurnEnded { reason });
            }
            _ => {}
        }
        None
    }
}

enum Decision {
    /// Answer without asking anyone.
    Reply(Value),
    /// Show a card and wait.
    Ask(AgentEvent, Pending),
}

struct Pending {
    input: Value,
    suggestions: Value,
    kind: PendingKind,
}

enum PendingKind {
    Tool,
    Plan,
    Questions(Vec<Question>),
}

impl Pending {
    fn response(&self, choice: PermChoice) -> Value {
        match (choice, &self.kind) {
            (PermChoice::Deny, PendingKind::Plan) => json!({
                "behavior": "deny",
                "message": "The user wants to keep planning. Revise the plan before making changes.",
            }),
            (PermChoice::Deny, _) => {
                json!({ "behavior": "deny", "message": "The user denied this action." })
            }
            (PermChoice::AllowAlways, PendingKind::Tool) if self.suggestions.is_array() => json!({
                "behavior": "allow",
                "updatedInput": self.input,
                "updatedPermissions": self.suggestions,
            }),
            _ => json!({ "behavior": "allow", "updatedInput": self.input }),
        }
    }

    /// The tool input with the user's answers keyed by question text: a
    /// string per single-select question, an array per multi-select one.
    fn answer(&self, answers: &[Vec<String>]) -> Value {
        let PendingKind::Questions(questions) = &self.kind else {
            return self.response(PermChoice::Deny);
        };
        let mut by_question = serde_json::Map::new();
        for (ix, question) in questions.iter().enumerate() {
            let labels = answers.get(ix).cloned().unwrap_or_default();
            let value = if question.multi {
                Value::from(labels)
            } else {
                Value::from(labels.into_iter().next().unwrap_or_default())
            };
            by_question.insert(question.text.clone(), value);
        }
        let mut input = self.input.as_object().cloned().unwrap_or_default();
        input.insert("answers".into(), Value::Object(by_question));
        json!({ "behavior": "allow", "updatedInput": input })
    }
}

fn decide(request: &ControlRequest, mode: Mode, unattended: bool) -> Decision {
    let deny = |message: &str| Decision::Reply(json!({ "behavior": "deny", "message": message }));
    if unattended {
        return deny("This task runs unattended, so actions that need approval are denied.");
    }
    let req_id = request.request_id.clone();
    let pending = |kind| Pending {
        input: request.input.clone(),
        suggestions: request.suggestions.clone(),
        kind,
    };
    match request.tool_name.as_str() {
        "AskUserQuestion" => {
            let questions = parse_questions(&request.input);
            Decision::Ask(
                AgentEvent::Question {
                    req_id,
                    questions: questions.clone(),
                },
                pending(PendingKind::Questions(questions)),
            )
        }
        "ExitPlanMode" => Decision::Ask(
            AgentEvent::PermissionRequest {
                req_id,
                title: "Approve this plan?".into(),
                detail: string(&request.input["plan"]),
                plan: true,
            },
            pending(PendingKind::Plan),
        ),
        name if mode == Mode::Ask && WRITE_TOOLS.contains(&name) => {
            deny("Ask mode is read-only. Switch to Agent mode to make changes.")
        }
        name => Decision::Ask(
            AgentEvent::PermissionRequest {
                req_id,
                title: tool_title(name, &request.input),
                detail: permission_detail(name, &request.input, &request.description),
                plan: false,
            },
            pending(PendingKind::Tool),
        ),
    }
}

fn parse_questions(input: &Value) -> Vec<Question> {
    input["questions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(ix, q)| Question {
            id: ix.to_string(),
            header: q["header"].as_str().unwrap_or("Question").to_owned(),
            text: string(&q["question"]),
            options: q["options"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|option| match option {
                    Value::String(label) => label.clone(),
                    other => string(&other["label"]),
                })
                .collect(),
            multi: q["multiSelect"] == true,
        })
        .collect()
}

fn todos(input: &Value) -> Vec<TodoItem> {
    input["todos"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|todo| TodoItem {
            text: string(&todo["content"]),
            status: match todo["status"].as_str() {
                Some("completed") => TodoStatus::Done,
                Some("in_progress") => TodoStatus::InProgress,
                _ => TodoStatus::Pending,
            },
        })
        .collect()
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Read" | "NotebookRead" => ToolKind::Read,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "Bash" | "BashOutput" | "KillShell" | "PowerShell" => ToolKind::Execute,
        "Grep" | "Glob" | "LS" => ToolKind::Search,
        "WebFetch" | "WebSearch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_title(name: &str, input: &Value) -> String {
    let arg = [
        "command",
        "file_path",
        "notebook_path",
        "pattern",
        "url",
        "query",
        "description",
        "path",
    ]
    .iter()
    .find_map(|key| input[*key].as_str());
    match arg {
        Some(arg) => clip(&format!("{name}: {arg}"), 200),
        None => name.to_owned(),
    }
}

fn permission_detail(name: &str, input: &Value, description: &str) -> String {
    let body = match name {
        "Bash" | "PowerShell" => format!("```sh\n{}\n```", string(&input["command"])),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => format!(
            "`{}`",
            input["file_path"]
                .as_str()
                .or(input["notebook_path"].as_str())
                .unwrap_or_default()
        ),
        _ => format!(
            "```json\n{}\n```",
            clip(
                &serde_json::to_string_pretty(input).unwrap_or_default(),
                2000
            )
        ),
    };
    if description.is_empty() {
        body
    } else {
        format!("{description}\n\n{body}")
    }
}

fn blocks(content: &Value) -> &[Value] {
    content.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn string(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn permission_mode(settings: &TurnSettings) -> &'static str {
    match (settings.mode, settings.access) {
        (Mode::Plan, _) => "plan",
        (_, Access::Supervised) => "default",
        (_, Access::AutoEdits) => "acceptEdits",
        (_, Access::FullAccess) => "bypassPermissions",
    }
}

fn user_line(text: &str, steer: bool) -> String {
    let mut line = json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
    });
    if steer {
        // Answered at the next step of the running turn.
        line["priority"] = json!("now");
    }
    line.to_string()
}

fn control_request(id: u64, request: Value) -> String {
    json!({ "type": "control_request", "request_id": format!("sorrel-{id}"), "request": request })
        .to_string()
}

fn control_response(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
    .to_string()
}

fn mcp_config(servers: &[McpServer]) -> String {
    let servers: serde_json::Map<String, Value> = servers
        .iter()
        .map(|server| {
            let env: serde_json::Map<String, Value> = server
                .env
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                .collect();
            let config = json!({ "command": server.command, "args": server.args, "env": env });
            (server.name.clone(), config)
        })
        .collect();
    json!({ "mcpServers": servers }).to_string()
}

struct Process {
    _child: Child,
    stdin: ChildStdin,
    // ponytail: one stdout line is held whole; tool output beyond the preview goes to blobs.
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: StderrTail,
}

impl Process {
    fn spawn(
        cfg: &SessionConfig,
        resume: Option<&str>,
        settings: &TurnSettings,
    ) -> Result<Self, String> {
        let mut cmd = crate::command(&cfg.bin);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--permission-mode",
            permission_mode(settings),
        ]);
        if cfg.unattended {
            cmd.args(["--permission-prompts", "none"]);
        } else {
            // Lets the user switch to Full access later without a restart.
            cmd.args([
                "--permission-prompt-tool",
                "stdio",
                "--allow-dangerously-skip-permissions",
            ]);
        }
        if let Some(model) = &settings.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &settings.effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(id) = resume {
            cmd.args(["--resume", id]);
        }
        if !cfg.mcp_servers.is_empty() {
            cmd.args(["--mcp-config", &mcp_config(&cfg.mcp_servers)]);
        }
        if let Some(key) = &cfg.api_key {
            cmd.env("ANTHROPIC_API_KEY", key);
        }
        cmd.current_dir(&cfg.cwd);
        let mut child = cmd.spawn().map_err(|e| crate::spawn_error(&cfg.bin, &e))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
        let stderr = StderrTail::drain(child.stderr.take().expect("stderr is piped"));
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            stderr,
        })
    }

    async fn write(&mut self, line: &str) -> io::Result<()> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }
}

async fn next_line(process: &mut Option<Process>) -> Option<String> {
    match process {
        Some(p) => p.stdout.next_line().await.ok().flatten(),
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_keyed_by_question_text() {
        let input = json!({ "questions": [
            { "question": "Pick one", "header": "H", "options": [{ "label": "A" }, { "label": "B" }], "multiSelect": false },
            { "question": "Pick many", "header": "H", "options": ["X", "Y"], "multiSelect": true },
        ]});
        let questions = parse_questions(&input);
        assert_eq!(questions[1].options, vec!["X", "Y"]);
        let pending = Pending {
            input: input.clone(),
            suggestions: Value::Null,
            kind: PendingKind::Questions(questions),
        };
        let reply = pending.answer(&[vec!["B".into()], vec!["X".into(), "Y".into()]]);
        assert_eq!(reply["behavior"], "allow");
        assert_eq!(reply["updatedInput"]["answers"]["Pick one"], "B");
        assert_eq!(
            reply["updatedInput"]["answers"]["Pick many"],
            json!(["X", "Y"])
        );
        assert_eq!(reply["updatedInput"]["questions"], input["questions"]);
    }

    #[test]
    fn ask_mode_denies_edits_without_asking() {
        let request = ControlRequest {
            request_id: "r".into(),
            tool_name: "Edit".into(),
            input: json!({ "file_path": "a.rs" }),
            suggestions: Value::Null,
            description: String::new(),
        };
        assert!(
            matches!(decide(&request, Mode::Ask, false), Decision::Reply(r) if r["behavior"] == "deny")
        );
        assert!(matches!(
            decide(&request, Mode::Agent, false),
            Decision::Ask(AgentEvent::PermissionRequest { .. }, _)
        ));
        assert!(matches!(
            decide(&request, Mode::Agent, true),
            Decision::Reply(_)
        ));
    }
}

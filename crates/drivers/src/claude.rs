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
    Access, AgentEvent, AuthState, LimitWindow, McpServer, Mode, ModelInfo, MsgId, PermChoice,
    Question, StopReason, TodoItem, TodoStatus, ToolKind, ToolStatus, TurnSettings, ULTRACODE,
    ULTRATHINK, wait_text,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc,
    time::{Instant, sleep_until},
};

use crate::{
    DriverCommand, SessionConfig, StderrTail, blob, clip, diff_lines, fail, now_secs,
    unix_from_rfc3339,
};

/// Context window sizes `claude` runs with: the default, and with `[1m]`.
const WINDOW: u64 = 200_000;
const LONG_WINDOW: u64 = 1_000_000;
/// Lines of a diff or written file shown on a tool call.
const DETAIL_LINES: usize = 160;

/// Tools Ask mode refuses without asking.
const WRITE_TOOLS: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];

/// The picker's fallback when the CLI can't be asked: its current models.
pub fn models() -> Vec<ModelInfo> {
    let efforts: Vec<String> = [
        "low", "medium", "high", "xhigh", "max", ULTRATHINK, ULTRACODE,
    ]
    .map(String::from)
    .to_vec();
    [
        // The CLI's default first, as `discover_models` leaves it.
        ("claude-opus-5-5", "Opus 5.5", "Best for everyday, complex tasks"),
        ("claude-fable-5-1", "Fable 5.1", "Most capable for your hardest tasks"),
        ("claude-sonnet-5-5", "Sonnet 5.5", "Efficient for routine tasks"),
        ("claude-haiku-4-5-20251001", "Haiku 4.5", "Fastest for quick answers"),
    ]
    .into_iter()
    .map(|(id, label, description)| ModelInfo {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        efforts: if id.contains("haiku") {
            Vec::new()
        } else {
            efforts.clone()
        },
        fast: id.contains("opus"),
    })
    .collect()
}

/// Every model the CLI's own `/model` picker offers, older ones included,
/// read from its `initialize` handshake. Works signed out too.
pub async fn discover_models(
    bin: PathBuf,
    api_key: Option<String>,
    env: Vec<(String, String)>,
) -> Result<Vec<ModelInfo>, String> {
    parse_models(&query(bin, api_key, env, "initialize").await?)
}

/// A short title for a conversation that opens with `message`, from a
/// one-shot Haiku call with no tools, settings, hooks or saved session.
pub async fn title(
    bin: PathBuf,
    api_key: Option<String>,
    env: Vec<(String, String)>,
    message: &str,
) -> Result<String, String> {
    let mut cmd = crate::command(&bin);
    cmd.args([
        "--print",
        "--model",
        "haiku",
        "--tools",
        "",
        "--setting-sources",
        "",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--system-prompt",
        "You name chat conversations. Reply with a title of 2 to 6 words in the language of the \
         user's message: no quotes, no trailing period, nothing else.",
    ])
    .envs(env)
    .current_dir(std::env::temp_dir());
    if let Some(key) = &api_key {
        cmd.env("ANTHROPIC_API_KEY", key);
    }
    let mut child = cmd.spawn().map_err(|e| crate::spawn_error(&bin, &e))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let prompt = format!("Title this conversation:\n\n{}", clip(message, 2000));
    stdin
        .write_all(prompt.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| "claude took too long to title the thread".to_owned())?
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let title = text
        .lines()
        .map(|l| l.trim().trim_matches(['"', '\'', '*', '#', '`']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_end_matches('.');
    if !output.status.success() || title.is_empty() {
        return Err("claude gave no title".into());
    }
    Ok(title.chars().take(60).collect())
}

/// The subscription's usage limits, from the `get_usage` control request.
/// Costs no tokens.
pub async fn usage_limits(
    bin: PathBuf,
    api_key: Option<String>,
    env: Vec<(String, String)>,
) -> Result<Vec<LimitWindow>, String> {
    let response = query(bin, api_key, env, "get_usage").await?;
    if response["subtype"] == "error" {
        return Err(string(&response["error"]));
    }
    Ok(parse_usage(&response["response"]))
}

fn parse_usage(usage: &Value) -> Vec<LimitWindow> {
    let limits = &usage["rate_limits"];
    if usage["rate_limits_available"] != true || !limits.is_object() {
        return Vec::new();
    }
    LIMIT_KEYS
        .into_iter()
        .filter_map(|(key, label)| {
            let window = &limits[key];
            Some(LimitWindow {
                label: label.into(),
                used: window["utilization"].as_f64()? as f32,
                resets_at: window["resets_at"].as_str().map_or(0, unix_from_rfc3339),
            })
        })
        .collect()
}

/// Usage-limit windows by the CLI's name for them, in display order.
const LIMIT_KEYS: [(&str, &str); 4] = [
    ("five_hour", "Session"),
    ("seven_day", "Weekly"),
    ("seven_day_opus", "Weekly (Opus)"),
    ("seven_day_sonnet", "Weekly (Sonnet)"),
];

/// Starts `claude`, sends one control request (after `initialize`, which the
/// others need) and returns its response. The process dies on return.
async fn query(
    bin: PathBuf,
    api_key: Option<String>,
    env: Vec<(String, String)>,
    subtype: &str,
) -> Result<Value, String> {
    let mut cmd = crate::command(&bin);
    cmd.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
    ])
    .stderr(Stdio::null())
    .envs(env)
    .current_dir(std::env::temp_dir());
    if let Some(key) = &api_key {
        cmd.env("ANTHROPIC_API_KEY", key);
    }
    // Dropped (and so killed) on return.
    let mut child = cmd.spawn().map_err(|e| crate::spawn_error(&bin, &e))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
    let mut requests = vec!["initialize"];
    if subtype != "initialize" {
        requests.push(subtype);
    }
    for id in requests {
        let request = json!({
            "type": "control_request",
            "request_id": id,
            "request": { "subtype": id },
        });
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    let read = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v["type"] == "control_response" && v["response"]["request_id"] == subtype {
                return Ok(v["response"].clone());
            }
        }
        Err(format!("claude closed before it answered {subtype}"))
    };
    tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .map_err(|_| format!("claude took too long to answer {subtype}"))?
}

/// The models in an `initialize` response, aliases resolved to concrete ids.
fn parse_models(response: &Value) -> Result<Vec<ModelInfo>, String> {
    if response["subtype"] == "error" {
        return Err(string(&response["error"]));
    }
    let mut models: Vec<ModelInfo> = Vec::new();
    for entry in response["response"]["models"].as_array().into_iter().flatten() {
        let value = entry["value"].as_str().unwrap_or_default();
        // The picker's own "Default" row stands for this one.
        if value == "default" {
            continue;
        }
        let id = entry["resolvedModel"]
            .as_str()
            .filter(|id| !id.is_empty())
            .unwrap_or(value);
        if id.is_empty() || models.iter().any(|m| m.id == id) {
            continue;
        }
        let mut efforts: Vec<String> = entry["supportedEffortLevels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().map(str::to_owned))
            .collect();
        // Ultrathink is a prompt prefix any thinking model takes; Ultracode
        // runs at xhigh, so only where xhigh exists.
        let xhigh = efforts.iter().any(|e| e == "xhigh");
        if !efforts.is_empty() {
            efforts.push(ULTRATHINK.into());
        }
        if xhigh {
            efforts.push(ULTRACODE.into());
        }
        models.push(ModelInfo {
            id: id.into(),
            label: entry["displayName"].as_str().unwrap_or(id).into(),
            description: string(&entry["description"]),
            efforts,
            fast: entry["supportsFastMode"] == true,
        });
    }
    if models.is_empty() {
        return Err("claude listed no models".into());
    }
    Ok(models)
}

/// Asks `claude auth status`: state, detail (the plan when signed in), and
/// the account's email. Never reads credential files.
pub async fn auth_status(bin: &Path) -> (AuthState, String, String) {
    let mut cmd = crate::command(bin);
    cmd.args(["auth", "status"]).stdin(Stdio::null());
    let output = match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Err(_) => {
            return (
                AuthState::Unknown,
                "`claude auth status` timed out".into(),
                String::new(),
            );
        }
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
            return (AuthState::Missing, crate::spawn_error(bin, &e), String::new());
        }
        Ok(Err(e)) => return (AuthState::Unknown, e.to_string(), String::new()),
        Ok(Ok(output)) => output,
    };
    let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let method = status["authMethod"].as_str().unwrap_or_default().to_owned();
    let email = status["email"].as_str().unwrap_or_default().to_owned();
    let plan = match status["subscriptionType"].as_str() {
        Some(plan) if !plan.is_empty() => {
            let mut chars = plan.chars();
            chars.next().map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
        }
        _ => method.clone(),
    };
    match status["loggedIn"].as_bool() {
        Some(true) if method.to_ascii_lowercase().contains("key") => {
            (AuthState::ApiKey, method, email)
        }
        Some(true) => (AuthState::Subscription, plan, email),
        Some(false) => (AuthState::SignedOut, "Not signed in".into(), String::new()),
        None => (
            AuthState::Unknown,
            String::from_utf8_lossy(&output.stderr).into_owned(),
            String::new(),
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
        (AuthState::Missing, detail, _) => Err(detail),
        (AuthState::SignedOut, ..) => Err(
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
                        // Effort, fast mode, Ultracode and the context window are fixed when
                        // claude starts: switch them between turns by restarting.
                        if process.is_some() && !in_turn && Launch::of(&wanted) != Launch::of(&current) {
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
                                    translator.window = if wanted.long_context { LONG_WINDOW } else { WINDOW };
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
                        if permission_mode(&wanted, restricted(&cfg)) != permission_mode(&current, restricted(&cfg)) {
                            next_request += 1;
                            let request = json!({ "subtype": "set_permission_mode", "mode": permission_mode(&wanted, restricted(&cfg)) });
                            written = running.write(&control_request(next_request, request)).await;
                        }
                        if written.is_ok() && wanted.model != current.model {
                            next_request += 1;
                            let request = json!({ "subtype": "set_model", "model": model_name(&wanted) });
                            written = running.write(&control_request(next_request, request)).await;
                        }
                        let ultrathink = wanted.effort.as_deref() == Some(ULTRATHINK);
                        current = TurnSettings { effort: current.effort.clone(), ..wanted };
                        if written.is_ok() {
                            let text = if ultrathink { ultrathink_prompt(&text) } else { text };
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
    /// The context window's size; each result names the real one.
    window: u64,
    /// Context tokens in use after the latest model call.
    context: u64,
    /// Usage-limit notices already shown, by window and reset time.
    announced: Vec<(String, i64)>,
}

impl Translator {
    pub fn new(blob_dir: PathBuf) -> Self {
        Self {
            msg_id: MsgId::new(),
            blob_dir,
            window: WINDOW,
            context: 0,
            announced: Vec::new(),
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
            Some("system") if v["subtype"] == "init" => {
                out.push(AgentEvent::SessionStarted {
                    session_id: string(&v["session_id"]),
                    model: string(&v["model"]),
                });
                if let Some(names) = v["slash_commands"].as_array().filter(|n| !n.is_empty()) {
                    out.push(AgentEvent::Commands {
                        names: names
                            .iter()
                            .filter_map(|n| n.as_str().map(str::to_owned))
                            .collect(),
                    });
                }
            }
            Some("stream_event") if main => {
                let event = &v["event"];
                match event["type"].as_str() {
                    Some("message_start") => {
                        let message = &event["message"];
                        self.msg_id = string(&message["id"]);
                        let usage = &message["usage"];
                        let tokens = |key: &str| usage[key].as_u64().unwrap_or(0);
                        let used = tokens("input_tokens")
                            + tokens("cache_creation_input_tokens")
                            + tokens("cache_read_input_tokens");
                        if used > 0 {
                            self.context = used;
                            out.push(AgentEvent::Context {
                                used,
                                window: self.window,
                            });
                        }
                    }
                    Some("content_block_delta") => {
                        let delta = &event["delta"];
                        // Hidden thinking streams as empty deltas; they would make empty rows.
                        if delta["text"] == "" || delta["thinking"] == "" {
                            return None;
                        }
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
                            detail: tool_detail(name, &block["input"]),
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
            Some("rate_limit_event") => self.rate_limit(&v["rate_limit_info"], out),
            Some("result") => {
                // The CLI names the real window here; correct the meter if it differs.
                let window = v["modelUsage"]
                    .as_object()
                    .into_iter()
                    .flat_map(|models| models.values())
                    .filter_map(|m| m["contextWindow"].as_u64())
                    .max();
                if let Some(window) = window.filter(|&w| w != self.window) {
                    self.window = window;
                    if self.context > 0 {
                        out.push(AgentEvent::Context {
                            used: self.context,
                            window,
                        });
                    }
                }
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

impl Translator {
    /// Live usage limits, and a notice the first time a window blocks the turn.
    fn rate_limit(&mut self, info: &Value, out: &mut Vec<AgentEvent>) {
        let label = |key: &str| {
            LIMIT_KEYS
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, label)| *label)
        };
        // `unifiedWindows` carries every window at once; older CLIs name only one.
        let mut windows: Vec<LimitWindow> = LIMIT_KEYS
            .iter()
            .filter_map(|(key, label)| {
                let w = &info["unifiedWindows"][*key];
                Some(LimitWindow {
                    label: (*label).into(),
                    used: (w["utilization"].as_f64()? * 100.) as f32,
                    resets_at: w["resetsAt"].as_i64().unwrap_or(0),
                })
            })
            .collect();
        let kind = info["rateLimitType"].as_str().unwrap_or_default();
        if windows.is_empty()
            && let (Some(label), Some(used)) = (label(kind), info["utilization"].as_f64())
        {
            windows.push(LimitWindow {
                label: label.into(),
                used: (used * 100.) as f32,
                resets_at: info["resetsAt"].as_i64().unwrap_or(0),
            });
        }
        if !windows.is_empty() {
            out.push(AgentEvent::Limits { windows });
        }
        let overage = matches!(
            info["overageStatus"].as_str(),
            Some("allowed" | "allowed_warning")
        ) || info["isUsingOverage"] == true;
        if info["status"] != "rejected" || overage {
            return;
        }
        let resets = info["resetsAt"].as_i64().unwrap_or(0);
        let key = (kind.to_owned(), resets);
        if self.announced.contains(&key) {
            return;
        }
        self.announced.push(key);
        let name = match kind {
            "five_hour" => "5-hour ",
            "seven_day" | "seven_day_opus" | "seven_day_sonnet" => "weekly ",
            _ => "",
        };
        let wait = resets - now_secs();
        let when = if wait > 0 && wait < 30 * 86_400 {
            format!(" in {}", wait_text(wait))
        } else {
            String::new()
        };
        out.push(AgentEvent::Error {
            message: format!("Claude usage limit reached. The {name}limit resets{when}."),
        });
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

/// What an edit or write will change, as diff lines; empty for other tools.
fn tool_detail(name: &str, input: &Value) -> String {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let mut lines = Vec::new();
    match name {
        "Edit" => {
            diff_lines(&mut lines, '-', &text(&input["old_string"]), DETAIL_LINES);
            diff_lines(&mut lines, '+', &text(&input["new_string"]), DETAIL_LINES);
        }
        "MultiEdit" => {
            for (ix, edit) in blocks(&input["edits"]).iter().enumerate() {
                if ix > 0 && lines.len() < DETAIL_LINES {
                    lines.push("@@".into());
                }
                diff_lines(&mut lines, '-', &text(&edit["old_string"]), DETAIL_LINES);
                diff_lines(&mut lines, '+', &text(&edit["new_string"]), DETAIL_LINES);
            }
        }
        "Write" => diff_lines(&mut lines, '+', &text(&input["content"]), DETAIL_LINES),
        "NotebookEdit" => diff_lines(&mut lines, '+', &text(&input["new_source"]), DETAIL_LINES),
        _ => {}
    }
    lines.join("\n")
}

fn permission_detail(name: &str, input: &Value, description: &str) -> String {
    let body = match name {
        "Bash" | "PowerShell" => format!("```sh\n{}\n```", string(&input["command"])),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let path = input["file_path"]
                .as_str()
                .or(input["notebook_path"].as_str())
                .unwrap_or_default();
            match tool_detail(name, input) {
                diff if diff.is_empty() => format!("`{path}`"),
                diff => format!("`{path}`\n\n```diff\n{diff}\n```"),
            }
        }
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

/// What `claude` takes at launch and keeps for the life of the process.
#[derive(PartialEq)]
struct Launch {
    effort: Option<String>,
    fast: bool,
    ultracode: bool,
    long_context: bool,
}

impl Launch {
    fn of(settings: &TurnSettings) -> Launch {
        let effort = match settings.effort.as_deref() {
            // Ultrathink is a word in the prompt, not a level: keep the model's own.
            Some(ULTRATHINK) => None,
            Some(ULTRACODE) => Some("xhigh".to_owned()),
            other => other.map(str::to_owned),
        };
        Launch {
            effort,
            fast: settings.fast,
            ultracode: settings.effort.as_deref() == Some(ULTRACODE),
            long_context: settings.long_context,
        }
    }

    /// Flag settings for `--settings`; `None` when there are none.
    fn settings_json(&self) -> Option<String> {
        let mut flags = serde_json::Map::new();
        if self.fast {
            flags.insert("fastMode".into(), Value::Bool(true));
        }
        if self.ultracode {
            flags.insert("ultracode".into(), Value::Bool(true));
        }
        (!flags.is_empty()).then(|| Value::Object(flags).to_string())
    }
}

/// Claude Code reasons as long as it needs on a turn that starts with the
/// keyword. Slash commands stay as they are, since a prefix would break them.
fn ultrathink_prompt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('/') || trimmed.starts_with("Ultrathink:") {
        trimmed.to_owned()
    } else {
        format!("Ultrathink:\n{trimmed}")
    }
}

/// The model as `claude` names it: `[1m]` asks for the long context window.
fn model_name(settings: &TurnSettings) -> Option<String> {
    let model = settings.model.as_deref()?;
    Some(if settings.long_context && !model.ends_with("[1m]") {
        format!("{model}[1m]")
    } else {
        model.to_owned()
    })
}

/// The system prompt that replaces Claude Code's own on the Chat side.
const CHAT_PROMPT: &str = "You are a helpful, friendly assistant in a desktop chat app. Answer directly and \
conversationally, in the language the user writes in. Use Markdown when it helps. Search the web when a \
question needs current information. When the user asks for a document, page or other file, write it into \
the current folder with the Write tool and say its file name; the app shows it as a card the user can open.";

/// Tools a Chat conversation may use: look things up and make files in its own folder.
const CHAT_TOOLS: &str = "WebSearch,WebFetch,Read,Write";

/// Restricted sessions (Chat, or `CLAUDE_CODE_RESTRICTED` in the environment)
/// refuse `bypassPermissions` and `--allow-dangerously-skip-permissions`.
fn restricted(cfg: &SessionConfig) -> bool {
    cfg.chat
        || cfg.args.iter().any(|a| a == "--restricted")
        || std::env::var("CLAUDE_CODE_RESTRICTED").is_ok_and(|v| {
            matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        })
}

fn permission_mode(settings: &TurnSettings, restricted: bool) -> &'static str {
    match (settings.mode, settings.access) {
        (Mode::Plan, _) => "plan",
        (_, Access::Supervised) => "default",
        (_, Access::AutoEdits) => "acceptEdits",
        (_, Access::Auto) => "auto",
        // ponytail: closest mode a restricted session allows.
        (_, Access::FullAccess) if restricted => "acceptEdits",
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
            permission_mode(settings, restricted(cfg)),
        ]);
        if cfg.unattended {
            cmd.args(["--permission-prompts", "none"]);
        } else {
            cmd.args(["--permission-prompt-tool", "stdio"]);
            if !restricted(cfg) {
                // Lets the user switch to Full access later without a restart.
                cmd.arg("--allow-dangerously-skip-permissions");
            }
        }
        let launch = Launch::of(settings);
        if let Some(model) = model_name(settings) {
            cmd.args(["--model", &model]);
        }
        if let Some(effort) = &launch.effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(flags) = launch.settings_json() {
            cmd.args(["--settings", &flags]);
        }
        if cfg.chat {
            // A general assistant: its own prompt, no shell or code edits, files only in its folder.
            cmd.args([
                "--system-prompt",
                CHAT_PROMPT,
                "--tools",
                CHAT_TOOLS,
                "--restricted",
            ]);
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
        cmd.args(&cfg.args)
            .envs(cfg.env.iter().map(|(k, v)| (k, v)));
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
    fn initialize_models_resolve_aliases_and_keep_older_ones() {
        let response = json!({ "subtype": "success", "response": { "models": [
            { "value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)" },
            { "value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus 5.5",
              "description": "Best for everyday, complex tasks",
              "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"] },
            { "value": "haiku", "resolvedModel": "claude-haiku-4-5-20251001", "displayName": "Haiku 4.5" },
            { "value": "claude-opus-4-6", "resolvedModel": "claude-opus-4-6", "displayName": "Opus 4.6",
              "supportedEffortLevels": ["low", "medium", "high", "max"] },
        ]}});
        let models = parse_models(&response).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["claude-opus-5-5", "claude-haiku-4-5-20251001", "claude-opus-4-6"]
        );
        assert_eq!(models[0].label, "Opus 5.5");
        assert_eq!(models[0].efforts.last().map(String::as_str), Some(ULTRACODE));
        assert!(models[1].efforts.is_empty());
        assert_eq!(models[2].efforts.last().map(String::as_str), Some(ULTRATHINK));
        assert!(parse_models(&json!({ "subtype": "error", "error": "offline" })).is_err());
    }

    #[test]
    fn chat_never_asks_for_bypass() {
        let mut cfg = SessionConfig::new("claude".into(), ".".into(), ".".into());
        let full = TurnSettings {
            access: Access::FullAccess,
            ..TurnSettings::default()
        };
        assert_eq!(permission_mode(&full, restricted(&cfg)), "bypassPermissions");
        cfg.chat = true;
        assert_eq!(permission_mode(&full, restricted(&cfg)), "acceptEdits");
    }

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

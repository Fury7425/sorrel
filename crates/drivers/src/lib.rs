//! One driver per CLI. Each spawns its CLI, speaks its wire format and emits
//! normalized [`proto::AgentEvent`]s. Vendor quirks stay in here.
//!
//! Every driver has the same shape: `run(config, commands, events)` drives one
//! thread's session until `commands` closes, spawning the CLI on demand,
//! killing it when a turn goes silent or the session idles, and resuming it by
//! session id on the next prompt.

pub mod acp;
pub mod blob;
pub mod claude;
pub mod codex;
mod rpc;

use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use proto::{AgentEvent, McpServer, Mode, PermChoice, StopReason, TurnSettings};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};

/// Everything a driver needs to run one thread's session.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub bin: PathBuf,
    pub cwd: PathBuf,
    /// The vendor session to resume: a claude session id, a codex thread id or
    /// an ACP session id.
    pub resume: Option<String>,
    /// Set only in API-key mode. Subscription logins stay inside each CLI.
    pub api_key: Option<String>,
    pub mcp_servers: Vec<McpServer>,
    /// The user's memory, for CLIs that cannot import a file.
    pub instructions: String,
    /// Scheduled tasks run with nobody watching, so anything that would ask
    /// for permission is denied.
    pub unattended: bool,
    pub blob_dir: PathBuf,
    /// Kill a turn that prints nothing for this long.
    pub turn_timeout: Duration,
    /// Kill a process that has had no turn for this long.
    pub idle_timeout: Duration,
    /// A Chat-side conversation: a general assistant with web search and a
    /// scratch folder for the pages it makes, not a coding agent.
    pub chat: bool,
    /// Extra CLI arguments and environment from the Providers page.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl SessionConfig {
    pub fn new(bin: PathBuf, cwd: PathBuf, blob_dir: PathBuf) -> Self {
        Self {
            bin,
            cwd,
            resume: None,
            api_key: None,
            mcp_servers: Vec::new(),
            instructions: String::new(),
            unattended: false,
            blob_dir,
            turn_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(600),
            chat: false,
            args: Vec::new(),
            env: Vec::new(),
        }
    }
}

/// What the engine asks of a running driver.
#[derive(Clone, Debug, PartialEq)]
pub enum DriverCommand {
    /// `steer` folds the text into the running turn instead of starting one.
    Prompt {
        text: String,
        settings: TurnSettings,
        steer: bool,
    },
    Interrupt,
    Resolve {
        req_id: String,
        choice: PermChoice,
    },
    Answer {
        req_id: String,
        answers: Vec<Vec<String>>,
    },
}

/// A text stand-in for a mode switch, for CLIs that have none.
pub(crate) fn mode_prefix(mode: Mode) -> &'static str {
    match mode {
        Mode::Agent => "",
        Mode::Plan => {
            "[Plan mode] Do not change any files. Investigate, then reply with a numbered plan and wait for approval.\n\n"
        }
        Mode::Ask => {
            "[Ask mode] Answer the question. Do not change files or run commands that modify anything.\n\n"
        }
    }
}

/// A child process with piped stdio that dies with its handle.
pub(crate) fn command(bin: &std::path::Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    // ponytail: kills the CLI itself, not tools it spawned; job objects / process groups later.
    cmd
}

/// The last lines a child wrote to stderr. Reading them also keeps the pipe
/// from filling up and stalling the child.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    const LINES: usize = 20;

    pub fn drain(stderr: tokio::process::ChildStderr) -> Self {
        let tail = StderrTail::default();
        let lines_tail = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = lines_tail.0.lock().unwrap();
                if tail.len() == Self::LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
        tail
    }

    pub fn text(&self) -> String {
        let tail = self.0.lock().unwrap();
        tail.iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The error a driver reports when it cannot start its CLI.
pub(crate) fn spawn_error(bin: &std::path::Path, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!(
            "{} was not found. Install it, or set SORREL_<NAME>_BIN to where it lives.",
            bin.display()
        )
    } else {
        format!("could not start {}: {e}", bin.display())
    }
}

pub(crate) async fn fail(events: &mpsc::Sender<AgentEvent>, message: String, reason: StopReason) {
    let _ = events.send(AgentEvent::Error { message }).await;
    let _ = events.send(AgentEvent::TurnEnded { reason }).await;
}

/// Shortens `s` to at most `max` bytes on a character boundary.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

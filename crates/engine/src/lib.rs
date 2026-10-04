//! The headless engine. It owns every CLI process and every file the app
//! writes: sessions and their supervision, the SQLite index and event log,
//! blob files, auth checks, project instructions, MCP connectors, checkpoints
//! and scheduled tasks.
//!
//! Clients send [`Request`]s and receive [`Update`]s, in the app's process or
//! over a local socket in daemon mode ([`daemon`]). One actor task handles
//! everything in order, so state needs no locks.

mod checkpoint;
pub mod daemon;
mod files;
mod store;

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use drivers::{DriverCommand, SessionConfig, acp, blob, claude, codex};
use proto::{
    AgentEvent, AuthState, AuthStatus, Delivery, McpServer, Mode, MsgId, ProjectId, ProjectInfo,
    Provider, Request, Seq, TaskId, ThreadEvent, ThreadId, ThreadInfo, Update,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

pub use files::data_dir;

use checkpoint::Shadow;
use store::{Store, now};

/// Rows per page when a thread opens or scrolls up.
const PAGE_ITEMS: usize = 50;
/// Idle driver tasks kept around; the oldest idle one goes beyond this.
const MAX_KEPT_SESSIONS: usize = 32;
const NEW_TITLE: &str = "New chat";

/// How clients reach a running engine. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    requests: mpsc::Sender<Request>,
    updates: broadcast::Sender<Update>,
}

impl Handle {
    pub fn requests(&self) -> mpsc::Sender<Request> {
        self.requests.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Update> {
        self.updates.subscribe()
    }
}

/// Starts the engine on the current tokio runtime.
pub fn start(data_dir: PathBuf) -> Result<Handle, String> {
    let dirs = Dirs::new(data_dir).map_err(|e| format!("cannot create the data folder: {e}"))?;
    let store = Store::open(&dirs.data.join("sorrel.db"))
        .map_err(|e| format!("cannot open the database: {e}"))?;
    let (requests, request_rx) = mpsc::channel(256);
    let (updates, _) = broadcast::channel(4096);
    let (agent_tx, agent_rx) = mpsc::channel(4096);
    let (notice_tx, notice_rx) = mpsc::channel(64);
    let engine = Engine {
        settings: files::Settings::load(&dirs.data),
        memory: fs::read_to_string(&dirs.memory).unwrap_or_default(),
        dirs,
        store,
        updates: updates.clone(),
        agent_tx,
        notice_tx,
        codex: None,
        sessions: HashMap::new(),
        running: 0,
        waiting: VecDeque::new(),
        checkpoints: true,
    };
    tokio::spawn(engine.run(request_rx, agent_rx, notice_rx));
    Ok(Handle { requests, updates })
}

struct Dirs {
    data: PathBuf,
    blobs: PathBuf,
    chats: PathBuf,
    projects: PathBuf,
    memory: PathBuf,
}

impl Dirs {
    fn new(data: PathBuf) -> std::io::Result<Dirs> {
        let dirs = Dirs {
            blobs: data.join("blobs"),
            chats: data.join("chats"),
            projects: data.join("projects"),
            memory: data.join("memory.md"),
            data,
        };
        for dir in [&dirs.data, &dirs.blobs, &dirs.chats, &dirs.projects] {
            fs::create_dir_all(dir)?;
        }
        Ok(dirs)
    }
}

/// A thread with a live driver task.
struct Session {
    commands: mpsc::Sender<DriverCommand>,
    /// A turn is running.
    running: bool,
    /// Waiting for a free session slot.
    waiting: bool,
    /// Messages to send after the running turn, in order.
    queue: VecDeque<(String, Mode)>,
    /// Streamed text not yet written to the log: message, thinking?, text.
    buffer: Option<(MsgId, bool, String)>,
    task: Option<TaskId>,
    last_used: Instant,
}

struct Engine {
    dirs: Dirs,
    store: Store,
    settings: files::Settings,
    memory: String,
    updates: broadcast::Sender<Update>,
    agent_tx: mpsc::Sender<(ThreadId, AgentEvent)>,
    notice_tx: mpsc::Sender<(String, Value)>,
    codex: Option<codex::Server>,
    sessions: HashMap<ThreadId, Session>,
    /// Turns running now, capped by `settings.max_sessions`.
    running: usize,
    waiting: VecDeque<ThreadId>,
    /// Off once git turned out to be missing.
    checkpoints: bool,
}

fn db<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("database error: {e}"))
}

impl Engine {
    async fn run(
        mut self,
        mut requests: mpsc::Receiver<Request>,
        mut agent_rx: mpsc::Receiver<(ThreadId, AgentEvent)>,
        mut notice_rx: mpsc::Receiver<(String, Value)>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    Some(request) => {
                        if let Err(message) = self.handle(request).await {
                            self.notice(message, true);
                        }
                    }
                    None => break,
                },
                Some((thread, event)) = agent_rx.recv() => self.on_agent(thread, event).await,
                Some((method, params)) = notice_rx.recv() => self.on_codex_notice(&method, &params),
                _ = tick.tick() => {
                    if let Err(message) = self.run_due_tasks().await {
                        self.notice(message, true);
                    }
                }
            }
        }
    }

    fn send(&self, update: Update) {
        let _ = self.updates.send(update);
    }

    fn notice(&self, message: impl Into<String>, error: bool) {
        self.send(Update::Notice {
            message: message.into(),
            error,
        });
    }

    async fn handle(&mut self, request: Request) -> Result<(), String> {
        match request {
            Request::Hello => {
                self.send(Update::Snapshot {
                    projects: self.projects()?,
                    threads: self.threads()?,
                    tasks: db(self.store.tasks())?,
                    auth: Provider::ALL
                        .iter()
                        .map(|&provider| AuthStatus {
                            provider,
                            state: AuthState::Unknown,
                            detail: "Checking…".into(),
                        })
                        .collect(),
                    settings: self.settings.view(&self.dirs.data),
                    memory: self.memory.clone(),
                });
                self.check_auth();
            }
            Request::CreateProject { name, folder } => {
                let name = if name.trim().is_empty() {
                    "Untitled project".to_owned()
                } else {
                    name.trim().to_owned()
                };
                let folder =
                    folder.unwrap_or_else(|| files::unique_dir(&self.dirs.projects, &name));
                fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
                db(self.store.create_project(&name, &folder))?;
                self.write_instructions(&folder, &files::read_instructions(&folder))?;
                self.send(Update::Projects(self.projects()?));
            }
            Request::UpdateProject {
                id,
                name,
                instructions,
            } => {
                let project =
                    db(self.store.project(id))?.ok_or("That project no longer exists.")?;
                db(self.store.rename_project(id, name.trim()))?;
                self.write_instructions(&project.folder, &instructions)?;
                self.send(Update::Projects(self.projects()?));
                self.notice("Project saved.", false);
            }
            Request::DeleteProject { id } => {
                db(self.store.delete_project(id))?;
                self.send(Update::Projects(self.projects()?));
                self.send(Update::Threads(self.threads()?));
                self.send(Update::Tasks(db(self.store.tasks())?));
                self.notice("Project removed. Its folder and files were kept.", false);
            }
            Request::CreateThread { project, provider } => {
                let id = self.create_thread(project, provider, NEW_TITLE)?;
                self.send(Update::Threads(self.threads()?));
                self.send(Update::Page {
                    thread: id,
                    events: Vec::new(),
                    turn: 0,
                    prepend: false,
                    older: None,
                });
            }
            Request::RenameThread { id, title } => {
                db(self.store.set_title(id, title.trim()))?;
                self.send(Update::Threads(self.threads()?));
            }
            Request::DeleteThread { id } => {
                if let Some(session) = self.sessions.remove(&id)
                    && session.running
                {
                    self.running -= 1;
                }
                self.waiting.retain(|&waiting| waiting != id);
                let thread = db(self.store.thread(id))?;
                db(self.store.delete_thread(id))?;
                if let Some(thread) = thread
                    && thread.folder.starts_with(&self.dirs.chats)
                {
                    let _ = fs::remove_dir_all(&thread.folder);
                }
                self.send(Update::Threads(self.threads()?));
            }
            Request::OpenThread { id } => self.page(id, None, false)?,
            Request::LoadOlder { id, before } => self.page(id, Some(before), true)?,
            Request::Send {
                thread,
                text,
                mode,
                delivery,
            } => self.send_message(thread, text, mode, delivery).await?,
            Request::Interrupt { thread } => {
                self.waiting.retain(|&waiting| waiting != thread);
                if let Some(session) = self.sessions.get_mut(&thread) {
                    session.queue.clear();
                    session.waiting = false;
                    let _ = session.commands.try_send(DriverCommand::Interrupt);
                }
                self.send(Update::Threads(self.threads()?));
            }
            Request::Resolve {
                thread,
                req_id,
                choice,
            } => {
                self.log(
                    thread,
                    ThreadEvent::PermissionResolved {
                        req_id: req_id.clone(),
                        choice,
                    },
                )?;
                self.command(thread, DriverCommand::Resolve { req_id, choice });
            }
            Request::Answer {
                thread,
                req_id,
                answers,
            } => {
                self.log(
                    thread,
                    ThreadEvent::QuestionAnswered {
                        req_id: req_id.clone(),
                        answers: answers.clone(),
                    },
                )?;
                self.command(thread, DriverCommand::Answer { req_id, answers });
            }
            Request::Restore { thread, turn } => self.restore(thread, turn).await?,
            Request::Diff { thread, turn } => {
                let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
                let (before, after) = db(self.store.checkpoint(thread, turn))?;
                let (Some(before), Some(after)) = (before, after) else {
                    return Err("There is no checkpoint for that turn.".into());
                };
                let shadow = Shadow::new(&self.dirs.data, &row.folder);
                let text = tokio::task::spawn_blocking(move || shadow.diff(&before, &after))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                let text = if text.is_empty() {
                    "No files changed in this turn.".to_owned()
                } else {
                    text
                };
                self.send(Update::Diff { thread, turn, text });
            }
            Request::ReadBlob { blob } => {
                let text = blob::read(&self.dirs.blobs, &blob)
                    .map_err(|e| format!("cannot read that output: {e}"))?;
                self.send(Update::Blob { blob, text });
            }
            Request::ListFiles { thread } => {
                let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
                let files = tokio::task::spawn_blocking(move || files::list_files(&row.folder))
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Update::Files { thread, files });
            }
            Request::ReadFile { thread, path } => {
                let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
                let content =
                    files::read_file(&row.folder, &path).map_err(|e| format!("{path}: {e}"))?;
                self.send(Update::File {
                    thread,
                    path,
                    content,
                });
            }
            Request::SetMemory { text } => {
                fs::write(&self.dirs.memory, &text).map_err(|e| e.to_string())?;
                self.memory = text;
                self.send(Update::Memory(self.memory.clone()));
                self.notice("Memory saved. New sessions pick it up.", false);
            }
            Request::SetApiKey { provider, key } => {
                match key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()) {
                    Some(key) => {
                        self.settings.api_keys.insert(provider.key().into(), key);
                        self.settings.use_api_key.insert(provider.key().into());
                    }
                    None => {
                        self.settings.api_keys.remove(provider.key());
                        self.settings.use_api_key.remove(provider.key());
                    }
                }
                self.settings_changed(provider)?;
            }
            Request::SetUseApiKey { provider, on } => {
                if on {
                    self.settings.use_api_key.insert(provider.key().into());
                } else {
                    self.settings.use_api_key.remove(provider.key());
                }
                self.settings_changed(provider)?;
            }
            Request::SetMcpServers { json } => {
                let servers: Vec<McpServer> = serde_json::from_str(&json)
                    .map_err(|e| format!("The connector list is not valid JSON: {e}"))?;
                self.settings.mcp_servers = servers;
                self.settings
                    .save(&self.dirs.data)
                    .map_err(|e| e.to_string())?;
                self.send(Update::Settings(self.settings.view(&self.dirs.data)));
                self.notice("Connectors saved. New sessions pick them up.", false);
            }
            Request::CheckAuth => self.check_auth(),
            Request::SignIn { provider } => self.sign_in(provider),
            Request::CreateTask {
                project,
                provider,
                prompt,
                every_minutes,
            } => {
                if prompt.trim().is_empty() {
                    return Err("A task needs a prompt.".into());
                }
                db(self.store.create_task(
                    project,
                    provider,
                    prompt.trim(),
                    every_minutes.filter(|&m| m > 0),
                ))?;
                self.send(Update::Tasks(db(self.store.tasks())?));
                self.run_due_tasks().await?;
            }
            Request::DeleteTask { id } => {
                db(self.store.delete_task(id))?;
                self.send(Update::Tasks(db(self.store.tasks())?));
            }
            Request::RunTask { id } => {
                db(self.store.set_task_run(id, now(), None, "queued"))?;
                self.run_due_tasks().await?;
            }
        }
        Ok(())
    }

    fn projects(&self) -> Result<Vec<ProjectInfo>, String> {
        Ok(db(self.store.projects())?
            .into_iter()
            .map(|p| ProjectInfo {
                instructions: files::read_instructions(&p.folder),
                id: p.id,
                name: p.name,
                folder: p.folder,
            })
            .collect())
    }

    fn threads(&self) -> Result<Vec<ThreadInfo>, String> {
        Ok(db(self.store.threads())?
            .into_iter()
            .map(|t| {
                let session = self.sessions.get(&t.id);
                ThreadInfo {
                    id: t.id,
                    project: t.project,
                    title: t.title,
                    provider: t.provider,
                    folder: t.folder,
                    running: session.is_some_and(|s| s.running || s.waiting),
                    queued: session.map_or(0, |s| s.queue.len()),
                    updated_at: t.updated_at,
                }
            })
            .collect())
    }

    fn write_instructions(&self, folder: &Path, instructions: &str) -> Result<(), String> {
        files::write_instructions(folder, instructions, &self.dirs.memory, &self.memory)
            .map_err(|e| format!("cannot write instructions in {}: {e}", folder.display()))
    }

    fn create_thread(
        &mut self,
        project: Option<ProjectId>,
        provider: Provider,
        title: &str,
    ) -> Result<ThreadId, String> {
        let folder = match project {
            Some(id) => {
                db(self.store.project(id))?
                    .ok_or("That project no longer exists.")?
                    .folder
            }
            None => self.dirs.chats.clone(),
        };
        let id = db(self.store.create_thread(project, title, provider, &folder))?;
        if project.is_none() {
            // Each chat gets its own scratch folder; its files are the chat's attachments.
            let folder = self.dirs.chats.join(id.to_string());
            fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
            db(self.store.set_thread_folder(id, &folder))?;
        }
        Ok(id)
    }

    fn page(&mut self, thread: ThreadId, before: Option<Seq>, prepend: bool) -> Result<(), String> {
        // Streamed text still in memory belongs in the page.
        self.flush(thread);
        let (events, turn, older) = db(self.store.page(thread, before, PAGE_ITEMS))?;
        self.send(Update::Page {
            thread,
            events,
            turn,
            prepend,
            older,
        });
        Ok(())
    }

    fn log(&mut self, thread: ThreadId, event: ThreadEvent) -> Result<(), String> {
        db(self.store.append(thread, &event))?;
        self.send(Update::Event { thread, event });
        Ok(())
    }

    fn command(&self, thread: ThreadId, command: DriverCommand) {
        if let Some(session) = self.sessions.get(&thread) {
            // ponytail: a full command queue means a wedged driver; the watchdog ends it.
            let _ = session.commands.try_send(command);
        }
    }

    fn settings_changed(&mut self, provider: Provider) -> Result<(), String> {
        self.settings
            .save(&self.dirs.data)
            .map_err(|e| e.to_string())?;
        if provider == Provider::Codex {
            // The shared app-server picks up the new key when it restarts.
            self.codex = None;
        }
        // Idle sessions of this provider restart with the new auth on their next message.
        self.sessions
            .retain(|_, session| session.running || session.waiting || !session.queue.is_empty());
        self.send(Update::Settings(self.settings.view(&self.dirs.data)));
        self.check_auth();
        Ok(())
    }

    fn codex_server(&mut self) -> codex::Server {
        self.codex
            .get_or_insert_with(|| {
                codex::Server::new(
                    files::resolve_bin(Provider::Codex),
                    self.settings.key_for(Provider::Codex),
                    self.notice_tx.clone(),
                )
            })
            .clone()
    }

    /// Spawns the thread's driver if it has none. Writes the folder's
    /// instruction files first, so every session starts with current memory.
    fn ensure_session(&mut self, thread: ThreadId, unattended: bool) -> Result<(), String> {
        if let Some(session) = self.sessions.get_mut(&thread) {
            session.last_used = Instant::now();
            return Ok(());
        }
        let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
        self.write_instructions(&row.folder, &files::read_instructions(&row.folder))?;

        let mut cfg = SessionConfig::new(
            files::resolve_bin(row.provider),
            row.folder.clone(),
            self.dirs.blobs.clone(),
        );
        cfg.resume = row.session;
        cfg.api_key = self.settings.key_for(row.provider);
        cfg.mcp_servers = self.settings.mcp_servers.clone();
        cfg.instructions = self.memory.clone();
        cfg.unattended = unattended;

        let (command_tx, command_rx) = mpsc::channel(32);
        let (event_tx, mut event_rx) = mpsc::channel(1024);
        match row.provider {
            Provider::Claude => {
                tokio::spawn(claude::run(cfg, command_rx, event_tx));
            }
            Provider::Codex => {
                let server = self.codex_server();
                tokio::spawn(codex::run(server, cfg, command_rx, event_tx));
            }
            other => {
                tokio::spawn(acp::run(cfg, acp::launch(other), command_rx, event_tx));
            }
        }
        let agent_tx = self.agent_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if agent_tx.send((thread, event)).await.is_err() {
                    break;
                }
            }
        });
        self.sessions.insert(
            thread,
            Session {
                commands: command_tx,
                running: false,
                waiting: false,
                queue: VecDeque::new(),
                buffer: None,
                task: None,
                last_used: Instant::now(),
            },
        );

        // Dropping a session's command channel ends its driver and kills its CLI.
        if self.sessions.len() > MAX_KEPT_SESSIONS {
            let idle = self
                .sessions
                .iter()
                .filter(|&(&id, s)| id != thread && !s.running && !s.waiting && s.queue.is_empty())
                .min_by_key(|(_, s)| s.last_used)
                .map(|(&id, _)| id);
            if let Some(id) = idle {
                self.sessions.remove(&id);
            }
        }
        Ok(())
    }

    async fn send_message(
        &mut self,
        thread: ThreadId,
        text: String,
        mode: Mode,
        delivery: Delivery,
    ) -> Result<(), String> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
        if row.title == NEW_TITLE {
            let title: String = text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(60)
                .collect();
            db(self.store.set_title(thread, &title))?;
        }
        self.ensure_session(thread, false)?;
        let session = self.sessions.get_mut(&thread).expect("ensured above");
        let (busy, running) = (session.running || session.waiting, session.running);
        if !busy {
            return self.start_turn(thread, text, mode).await;
        }
        if delivery == Delivery::SteerNow && running {
            self.log(
                thread,
                ThreadEvent::User {
                    text: text.clone(),
                    steer: true,
                },
            )?;
            self.command(
                thread,
                DriverCommand::Prompt {
                    text,
                    mode,
                    steer: true,
                },
            );
        } else if let Some(session) = self.sessions.get_mut(&thread) {
            session.queue.push_back((text, mode));
        }
        self.send(Update::Threads(self.threads()?));
        Ok(())
    }

    /// Starts a turn now, or parks it until a session slot frees up.
    async fn start_turn(
        &mut self,
        thread: ThreadId,
        text: String,
        mode: Mode,
    ) -> Result<(), String> {
        if self.running >= self.settings.max_sessions {
            if let Some(session) = self.sessions.get_mut(&thread) {
                session.queue.push_front((text, mode));
                session.waiting = true;
            }
            self.waiting.push_back(thread);
            self.notice(
                "Every session slot is busy; this message runs when one frees up.",
                false,
            );
            self.send(Update::Threads(self.threads()?));
            return Ok(());
        }
        let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
        let turn = row.turns + 1;
        let before = self.snapshot(thread, turn, "before", &row.folder).await;
        db(self
            .store
            .set_checkpoint(thread, turn, before.as_deref(), None))?;

        self.running += 1;
        if let Some(session) = self.sessions.get_mut(&thread) {
            session.running = true;
            session.waiting = false;
            session.last_used = Instant::now();
        }
        self.log(
            thread,
            ThreadEvent::User {
                text: text.clone(),
                steer: false,
            },
        )?;
        self.command(
            thread,
            DriverCommand::Prompt {
                text,
                mode,
                steer: false,
            },
        );
        self.send(Update::Threads(self.threads()?));
        Ok(())
    }

    async fn on_agent(&mut self, thread: ThreadId, event: AgentEvent) {
        if !self.sessions.contains_key(&thread) {
            return; // a deleted thread's driver winding down
        }
        let result = match event {
            AgentEvent::TextDelta { msg_id, text } => {
                self.buffer(thread, msg_id.clone(), false, &text);
                self.send(Update::Event {
                    thread,
                    event: ThreadEvent::Agent(AgentEvent::TextDelta { msg_id, text }),
                });
                Ok(())
            }
            AgentEvent::ThinkingDelta { msg_id, text } => {
                self.buffer(thread, msg_id.clone(), true, &text);
                self.send(Update::Event {
                    thread,
                    event: ThreadEvent::Agent(AgentEvent::ThinkingDelta { msg_id, text }),
                });
                Ok(())
            }
            event => {
                self.flush(thread);
                if let AgentEvent::SessionStarted { session_id, .. } = &event {
                    let _ = self.store.set_session(thread, session_id);
                }
                let ended = matches!(event, AgentEvent::TurnEnded { .. });
                let logged = self.log(thread, ThreadEvent::Agent(event));
                if ended {
                    self.end_turn(thread).await;
                }
                logged
            }
        };
        if let Err(message) = result {
            self.notice(message, true);
        }
    }

    /// Coalesces deltas so the log stores one row per stretch of text.
    fn buffer(&mut self, thread: ThreadId, msg_id: MsgId, thinking: bool, text: &str) {
        let continues = self
            .sessions
            .get(&thread)
            .and_then(|s| s.buffer.as_ref())
            .is_none_or(|(id, kind, _)| *id == msg_id && *kind == thinking);
        if !continues {
            self.flush(thread);
        }
        if let Some(session) = self.sessions.get_mut(&thread) {
            session
                .buffer
                .get_or_insert_with(|| (msg_id, thinking, String::new()))
                .2
                .push_str(text);
        }
    }

    fn flush(&mut self, thread: ThreadId) {
        let Some((msg_id, thinking, text)) =
            self.sessions.get_mut(&thread).and_then(|s| s.buffer.take())
        else {
            return;
        };
        let event = if thinking {
            AgentEvent::ThinkingDelta { msg_id, text }
        } else {
            AgentEvent::TextDelta { msg_id, text }
        };
        // Already shown live; only the log needs it.
        if let Err(e) = self.store.append(thread, &ThreadEvent::Agent(event)) {
            self.notice(format!("database error: {e}"), true);
        }
    }

    async fn end_turn(&mut self, thread: ThreadId) {
        let Some(session) = self.sessions.get_mut(&thread) else {
            return;
        };
        if !session.running {
            return;
        }
        session.running = false;
        session.last_used = Instant::now();
        let task = session.task.take();
        let next = session.queue.pop_front();
        self.running = self.running.saturating_sub(1);

        if let Ok(Some(row)) = self.store.thread(thread) {
            let after = self.snapshot(thread, row.turns, "after", &row.folder).await;
            let _ = self
                .store
                .set_checkpoint(thread, row.turns, None, after.as_deref());
        }
        if let Some(task) = task {
            let _ = self.store.set_task_status(task, "finished");
            if let Ok(tasks) = self.store.tasks() {
                self.send(Update::Tasks(tasks));
            }
        }

        // This thread's queue first, then whoever waited longest for a slot.
        let next = match next {
            Some((text, mode)) => Some((thread, text, mode)),
            None => self.waiting.pop_front().and_then(|waiting| {
                let session = self.sessions.get_mut(&waiting)?;
                session.waiting = false;
                session
                    .queue
                    .pop_front()
                    .map(|(text, mode)| (waiting, text, mode))
            }),
        };
        let started = match next {
            Some((thread, text, mode)) => Box::pin(self.start_turn(thread, text, mode)).await,
            None => self
                .threads()
                .map(|threads| self.send(Update::Threads(threads))),
        };
        if let Err(message) = started {
            self.notice(message, true);
        }
    }

    /// Snapshots the folder; `None` when checkpoints are off or git failed.
    async fn snapshot(
        &mut self,
        thread: ThreadId,
        turn: u32,
        label: &str,
        folder: &Path,
    ) -> Option<String> {
        if !self.checkpoints {
            return None;
        }
        let shadow = Shadow::new(&self.dirs.data, folder);
        let label = format!("{thread}/{turn}/{label}");
        match tokio::task::spawn_blocking(move || shadow.snapshot(&label)).await {
            Ok(Ok(commit)) => Some(commit),
            Ok(Err(e)) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    self.checkpoints = false;
                    self.notice(
                        "Git was not found, so turns have no checkpoints or undo.",
                        true,
                    );
                } else {
                    self.notice(format!("Checkpoint failed: {e}"), true);
                }
                None
            }
            Err(_) => None,
        }
    }

    async fn restore(&mut self, thread: ThreadId, turn: u32) -> Result<(), String> {
        if self.sessions.get(&thread).is_some_and(|s| s.running) {
            return Err("Stop the running turn before restoring files.".into());
        }
        let row = db(self.store.thread(thread))?.ok_or("That thread no longer exists.")?;
        let (before, _) = db(self.store.checkpoint(thread, turn))?;
        let before = before.ok_or("There is no checkpoint for that turn.")?;
        let shadow = Shadow::new(&self.dirs.data, &row.folder);
        let label = format!("{thread}/{turn}/restore-{}", now());
        tokio::task::spawn_blocking(move || shadow.restore(&before, &label))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("Restore failed: {e}"))?;
        self.log(thread, ThreadEvent::Restored { turn })?;
        self.notice(
            format!("Files restored to how they were before turn {turn}."),
            false,
        );
        Ok(())
    }

    fn check_auth(&self) {
        let updates = self.updates.clone();
        let keyed: Vec<Provider> = Provider::ALL
            .into_iter()
            .filter(|&p| self.settings.key_for(p).is_some())
            .collect();
        tokio::spawn(async move {
            let mut statuses = Vec::new();
            for provider in Provider::ALL {
                let bin = files::resolve_bin(provider);
                let (state, detail) = if keyed.contains(&provider) {
                    (AuthState::ApiKey, "Using your API key".to_owned())
                } else {
                    match provider {
                        Provider::Claude => claude::auth_status(&bin).await,
                        Provider::Codex => codex::login_status(&bin).await,
                        other => probe(&bin, acp::launch(other).sign_in).await,
                    }
                };
                statuses.push(AuthStatus {
                    provider,
                    state,
                    detail,
                });
            }
            let _ = updates.send(Update::Auth(statuses));
        });
    }

    fn sign_in(&mut self, provider: Provider) {
        match provider {
            Provider::Codex => {
                let server = self.codex_server();
                let updates = self.updates.clone();
                tokio::spawn(async move {
                    let update = match server.sign_in().await {
                        Ok(url) => Update::OpenUrl { url },
                        Err(message) => Update::Notice { message, error: true },
                    };
                    let _ = updates.send(update);
                });
            }
            Provider::Claude => self.notice(
                "Run `claude auth login` in a terminal, then press Check again. Sorrel never handles your Claude login.",
                false,
            ),
            other => self.notice(acp::launch(other).sign_in, false),
        }
    }

    fn on_codex_notice(&mut self, method: &str, params: &Value) {
        match method {
            "account/login/completed" if params["success"] == true => {
                self.notice("Signed in to Codex.", false);
                self.check_auth();
            }
            "account/login/completed" => {
                let error = params["error"].as_str().unwrap_or("unknown error");
                self.notice(format!("Codex sign-in failed: {error}"), true);
            }
            "account/updated" => self.check_auth(),
            _ => {}
        }
    }

    /// Starts every task whose time has come, each in its own unattended thread.
    async fn run_due_tasks(&mut self) -> Result<(), String> {
        let due = db(self.store.due_tasks(now()))?;
        if due.is_empty() {
            return Ok(());
        }
        for task in due {
            let thread = match task.thread {
                Some(thread) if db(self.store.thread(thread))?.is_some() => thread,
                _ => {
                    let title: String =
                        format!("Task: {}", task.prompt.chars().take(50).collect::<String>());
                    self.create_thread(task.project, task.provider, &title)?
                }
            };
            self.ensure_session(thread, true)?;
            let session = self.sessions.get_mut(&thread).expect("ensured above");
            if session.running || session.waiting {
                continue; // still busy with the last run; try again next tick
            }
            session.task = Some(task.id);
            let next_run = task
                .every_minutes
                .map_or(-1, |minutes| now() + i64::from(minutes) * 60);
            db(self
                .store
                .set_task_run(task.id, next_run, Some(thread), "running"))?;
            self.start_turn(thread, task.prompt.clone(), Mode::Agent)
                .await?;
        }
        self.send(Update::Tasks(db(self.store.tasks())?));
        self.send(Update::Threads(self.threads()?));
        Ok(())
    }
}

/// Whether an ACP agent's CLI is installed. Sign-in is checked when a session
/// starts, since ACP has no status call that does not start one.
async fn probe(bin: &Path, sign_in: &str) -> (AuthState, String) {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match tokio::time::timeout(Duration::from_secs(15), cmd.status()).await {
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => (
            AuthState::Missing,
            format!("{} was not found.", bin.display()),
        ),
        _ => (AuthState::Unknown, format!("Installed. {sign_in}")),
    }
}

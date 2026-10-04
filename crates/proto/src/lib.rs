//! The typed API between the UI and the engine.
//!
//! Requests go in, updates come out. Drivers translate each CLI's wire format
//! into [`AgentEvent`]s; the engine logs them as [`ThreadEvent`]s; anyone can
//! rebuild what a thread shows by replaying that log through [`Transcript`].
//! Nothing vendor-specific crosses this boundary.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type ProjectId = i64;
pub type ThreadId = i64;
pub type TaskId = i64;
/// One assistant message within a session (the vendor's message or item id).
pub type MsgId = String;
/// Position of an event in a thread's log.
pub type Seq = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    Claude,
    Codex,
    Cursor,
    Gemini,
    OpenCode,
}

impl Provider {
    pub const ALL: [Provider; 5] = [
        Provider::Claude,
        Provider::Codex,
        Provider::Cursor,
        Provider::Gemini,
        Provider::OpenCode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
            Provider::Gemini => "Gemini",
            Provider::OpenCode => "OpenCode",
        }
    }

    /// Stable name for storage and settings.
    pub fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Cursor => "cursor",
            Provider::Gemini => "gemini",
            Provider::OpenCode => "opencode",
        }
    }

    pub fn from_key(key: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.key() == key)
    }
}

/// Cursor-style modes: agent edits, plan proposes, ask only reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Agent,
    Plan,
    Ask,
}

/// How much an agent may do without asking (T3 Code's runtime modes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Access {
    /// Ask before commands and file changes.
    #[default]
    Supervised,
    /// Apply edits without asking; ask before anything else.
    AutoEdits,
    /// The CLI approves routine actions itself and asks about risky ones.
    Auto,
    /// Run commands and edits without prompts.
    FullAccess,
}

impl Access {
    pub const ALL: [Access; 4] = [
        Access::Supervised,
        Access::AutoEdits,
        Access::Auto,
        Access::FullAccess,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Access::Supervised => "Supervised",
            Access::AutoEdits => "Auto-accept edits",
            Access::Auto => "Auto",
            Access::FullAccess => "Full access",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Access::Supervised => "Ask before commands and file changes.",
            Access::AutoEdits => "Apply edits without asking, ask before other actions.",
            Access::Auto => "The CLI approves routine actions; risky ones still ask.",
            Access::FullAccess => "Allow commands and edits without prompts.",
        }
    }
}

/// What the composer picked for the next turn. Threads remember theirs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnSettings {
    /// `None` uses the CLI's default model.
    pub model: Option<String>,
    /// `None` uses the model's default reasoning effort.
    pub effort: Option<String>,
    pub mode: Mode,
    pub access: Access,
    /// Claude's fast mode: the same model with quicker output.
    pub fast: bool,
    /// The model's 1M-token context window instead of its default.
    pub long_context: bool,
}

/// Effort values past `max` that Claude Code takes as switches rather than
/// as `--effort` levels.
pub const ULTRATHINK: &str = "ultrathink";
pub const ULTRACODE: &str = "ultracode";

/// One entry in a provider's model list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Reasoning efforts the model accepts, if the CLI says.
    pub efforts: Vec<String>,
    /// Claude's fast mode works with this model.
    #[serde(default)]
    pub fast: bool,
}

/// What a message sent during a running turn does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// Run as the next turn.
    #[default]
    Queue,
    /// Fold into the running turn now.
    SteerNow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    Search,
    Fetch,
    Think,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

/// Content hash of a file in the engine's blob store.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermChoice {
    AllowOnce,
    AllowAlways,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub text: String,
    pub options: Vec<String>,
    pub multi: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    Interrupted,
    Error,
    /// The watchdog killed a turn that stopped producing output.
    Timeout,
}

/// One usage-limit window of a subscription, such as the 5-hour session or
/// the week.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    /// "Session", "Weekly", "Monthly".
    pub label: String,
    /// Percent of the window used, 0 to 100.
    pub used: f32,
    /// Unix seconds; 0 when the CLI did not say.
    pub resets_at: i64,
}

/// What every driver emits, whatever CLI is behind it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    SessionStarted {
        session_id: String,
        model: String,
    },
    TextDelta {
        msg_id: MsgId,
        text: String,
    },
    ThinkingDelta {
        msg_id: MsgId,
        text: String,
    },
    /// `detail` is what the call will change, as a unified diff for edits or
    /// the new text for a written file; empty when there is nothing to show.
    ToolCall {
        call_id: String,
        kind: ToolKind,
        title: String,
        #[serde(default)]
        detail: String,
    },
    /// `preview` is the first lines of the output; the rest lives in `output`.
    ToolUpdate {
        call_id: String,
        status: ToolStatus,
        preview: String,
        output: Option<BlobRef>,
    },
    /// Rendered as one card: allow once, allow always, or deny. `plan` marks a
    /// plan-approval request, whose `detail` is the plan in markdown.
    PermissionRequest {
        req_id: String,
        title: String,
        detail: String,
        plan: bool,
    },
    Question {
        req_id: String,
        questions: Vec<Question>,
    },
    Plan {
        items: Vec<TodoItem>,
    },
    Usage {
        input: u64,
        output: u64,
    },
    /// How full the context window is after the latest model call.
    Context {
        used: u64,
        window: u64,
    },
    /// The subscription's usage limits as the CLI last reported them. Not
    /// logged; the engine keeps the latest per CLI.
    Limits {
        windows: Vec<LimitWindow>,
    },
    TurnEnded {
        reason: StopReason,
    },
    Error {
        message: String,
    },
    /// The slash commands and skills the CLI offers. Not logged.
    Commands {
        names: Vec<String>,
    },
}

/// One entry in a thread's event log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ThreadEvent {
    /// `steer` messages join the running turn instead of starting one.
    User {
        text: String,
        steer: bool,
    },
    Agent(AgentEvent),
    PermissionResolved {
        req_id: String,
        choice: PermChoice,
    },
    QuestionAnswered {
        req_id: String,
        answers: Vec<Vec<String>>,
    },
    /// Files were put back to how they were before `turn`.
    Restored {
        turn: u32,
    },
}

impl ThreadEvent {
    /// Whether this event starts a new transcript row; pages begin on one.
    pub fn starts_item(&self) -> bool {
        match self {
            ThreadEvent::User { .. } | ThreadEvent::Restored { .. } => true,
            ThreadEvent::PermissionResolved { .. } | ThreadEvent::QuestionAnswered { .. } => false,
            ThreadEvent::Agent(event) => !matches!(
                event,
                AgentEvent::SessionStarted { .. }
                    | AgentEvent::ToolUpdate { .. }
                    | AgentEvent::Usage { .. }
                    | AgentEvent::Context { .. }
                    | AgentEvent::Limits { .. }
                    | AgentEvent::Commands { .. }
            ),
        }
    }
}

/// One row of a transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    User {
        text: String,
        turn: u32,
    },
    Assistant {
        msg_id: MsgId,
        text: String,
    },
    Thinking {
        msg_id: MsgId,
        text: String,
    },
    Tool {
        call_id: String,
        kind: ToolKind,
        title: String,
        detail: String,
        status: ToolStatus,
        preview: String,
        output: Option<BlobRef>,
    },
    Permission {
        req_id: String,
        title: String,
        detail: String,
        plan: bool,
        resolved: Option<PermChoice>,
    },
    Question {
        req_id: String,
        questions: Vec<Question>,
        answers: Option<Vec<Vec<String>>>,
    },
    Todo {
        items: Vec<TodoItem>,
    },
    Error {
        message: String,
    },
    TurnEnd {
        turn: u32,
        reason: StopReason,
        input: u64,
        output: u64,
    },
    Restored {
        turn: u32,
    },
}

/// The projection of a thread's log into rows. The UI applies live events to
/// it and the engine builds pages with it, so both always agree.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    pub items: Vec<Item>,
    /// Turns started so far (the first turn is 1).
    pub turn: u32,
    /// Context tokens in use and the window's size, as last reported.
    pub context: Option<(u64, u64)>,
    usage: (u64, u64),
    todo: Option<usize>,
}

impl Transcript {
    /// A transcript for a page whose first event comes after `turn` turns.
    pub fn starting_at(turn: u32) -> Self {
        Transcript {
            turn,
            ..Default::default()
        }
    }

    /// Applies one event; returns the index of the row it touched, if any.
    pub fn apply(&mut self, event: &ThreadEvent) -> Option<usize> {
        match event {
            ThreadEvent::User { text, steer } => {
                if !steer {
                    self.turn += 1;
                    self.todo = None;
                }
                self.push(Item::User {
                    text: text.clone(),
                    turn: self.turn,
                })
            }
            ThreadEvent::Restored { turn } => self.push(Item::Restored { turn: *turn }),
            ThreadEvent::PermissionResolved { req_id, choice } => {
                let ix = self.items.iter().rposition(
                    |item| matches!(item, Item::Permission { req_id: id, .. } if id == req_id),
                )?;
                if let Item::Permission { resolved, .. } = &mut self.items[ix] {
                    *resolved = Some(*choice);
                }
                Some(ix)
            }
            ThreadEvent::QuestionAnswered { req_id, answers } => {
                let ix = self.items.iter().rposition(
                    |item| matches!(item, Item::Question { req_id: id, .. } if id == req_id),
                )?;
                if let Item::Question { answers: slot, .. } = &mut self.items[ix] {
                    *slot = Some(answers.clone());
                }
                Some(ix)
            }
            ThreadEvent::Agent(event) => self.apply_agent(event),
        }
    }

    fn apply_agent(&mut self, event: &AgentEvent) -> Option<usize> {
        match event {
            AgentEvent::SessionStarted { .. }
            | AgentEvent::Commands { .. }
            | AgentEvent::Limits { .. } => None,
            AgentEvent::Context { used, window } => {
                self.context = Some((*used, *window));
                None
            }
            AgentEvent::TextDelta { msg_id, text } => {
                if let Some(Item::Assistant {
                    msg_id: id,
                    text: body,
                }) = self.items.last_mut()
                    && id == msg_id
                {
                    body.push_str(text);
                    return Some(self.items.len() - 1);
                }
                self.push(Item::Assistant {
                    msg_id: msg_id.clone(),
                    text: text.clone(),
                })
            }
            AgentEvent::ThinkingDelta { msg_id, text } => {
                if let Some(Item::Thinking {
                    msg_id: id,
                    text: body,
                }) = self.items.last_mut()
                    && id == msg_id
                {
                    body.push_str(text);
                    return Some(self.items.len() - 1);
                }
                self.push(Item::Thinking {
                    msg_id: msg_id.clone(),
                    text: text.clone(),
                })
            }
            AgentEvent::ToolCall {
                call_id,
                kind,
                title,
                detail,
            } => self.push(Item::Tool {
                call_id: call_id.clone(),
                kind: *kind,
                title: title.clone(),
                detail: detail.clone(),
                status: ToolStatus::Running,
                preview: String::new(),
                output: None,
            }),
            AgentEvent::ToolUpdate {
                call_id,
                status: new_status,
                preview: new_preview,
                output: new_output,
            } => {
                let ix = self.items.iter().rposition(
                    |item| matches!(item, Item::Tool { call_id: id, .. } if id == call_id),
                )?;
                if let Item::Tool {
                    status,
                    preview,
                    output,
                    ..
                } = &mut self.items[ix]
                {
                    *status = *new_status;
                    *preview = new_preview.clone();
                    *output = new_output.clone();
                }
                Some(ix)
            }
            AgentEvent::PermissionRequest {
                req_id,
                title,
                detail,
                plan,
            } => self.push(Item::Permission {
                req_id: req_id.clone(),
                title: title.clone(),
                detail: detail.clone(),
                plan: *plan,
                resolved: None,
            }),
            AgentEvent::Question { req_id, questions } => self.push(Item::Question {
                req_id: req_id.clone(),
                questions: questions.clone(),
                answers: None,
            }),
            // One live todo card per turn, updated in place.
            AgentEvent::Plan { items } => match self.todo {
                Some(ix) if ix < self.items.len() => {
                    self.items[ix] = Item::Todo {
                        items: items.clone(),
                    };
                    Some(ix)
                }
                _ => {
                    let ix = self.push(Item::Todo {
                        items: items.clone(),
                    });
                    self.todo = ix;
                    ix
                }
            },
            AgentEvent::Usage { input, output } => {
                self.usage = (*input, *output);
                None
            }
            AgentEvent::TurnEnded { reason } => {
                let (input, output) = std::mem::take(&mut self.usage);
                self.push(Item::TurnEnd {
                    turn: self.turn,
                    reason: *reason,
                    input,
                    output,
                })
            }
            AgentEvent::Error { message } => self.push(Item::Error {
                message: message.clone(),
            }),
        }
    }

    /// Puts an older page's rows above the current ones.
    pub fn prepend(&mut self, older: Vec<Item>) {
        let n = older.len();
        self.items.splice(0..0, older);
        self.todo = self.todo.map(|ix| ix + n);
    }

    fn push(&mut self, item: Item) -> Option<usize> {
        self.items.push(item);
        Some(self.items.len() - 1)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    pub folder: PathBuf,
    pub instructions: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub project: Option<ProjectId>,
    pub title: String,
    pub provider: Provider,
    pub folder: PathBuf,
    pub running: bool,
    /// A permission request or question is waiting on the user.
    pub needs_input: bool,
    /// Messages waiting for the running turn to end, in order.
    pub queue: Vec<String>,
    pub updated_at: i64,
    pub pinned: bool,
    pub archived: bool,
    /// The last turn ended in an error.
    pub failed: bool,
    /// The composer's last choices for this thread.
    pub settings: TurnSettings,
    /// A plain conversation (Chat) rather than a coding session (Code).
    pub chat: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: TaskId,
    pub project: Option<ProjectId>,
    pub provider: Provider,
    pub prompt: String,
    /// `None` runs once.
    pub every_minutes: Option<u32>,
    /// Unix seconds.
    pub next_run: i64,
    pub thread: Option<ThreadId>,
    pub last_status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthState {
    Unknown,
    /// The CLI is not installed or not on PATH.
    Missing,
    SignedOut,
    Subscription,
    ApiKey,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthStatus {
    pub provider: Provider,
    pub state: AuthState,
    pub detail: String,
    /// Where the CLI was found; empty when it was not.
    #[serde(default)]
    pub bin: String,
    /// First line of `--version`.
    #[serde(default)]
    pub version: String,
    /// The signed-in account's email, when the CLI reports it.
    #[serde(default)]
    pub account: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// How Sorrel runs one CLI, set on the Providers page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Hidden from pickers and never started.
    pub disabled: bool,
    /// Empty finds the CLI on PATH.
    pub binary: String,
    /// Extra arguments, split on whitespace.
    pub args: String,
    pub env: Vec<(String, String)>,
}

/// Which side of the app opens first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenIn {
    Chat,
    #[default]
    Code,
    Last,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Textures the wallpaper can be drawn with, after Zeron's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Effect {
    None,
    #[default]
    Scanlines,
    Dither,
    Halftone,
    Ascii,
}

impl Effect {
    pub const ALL: [Effect; 5] = [
        Effect::None,
        Effect::Scanlines,
        Effect::Dither,
        Effect::Halftone,
        Effect::Ascii,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Effect::None => "None",
            Effect::Scanlines => "Scanlines",
            Effect::Dither => "Dither",
            Effect::Halftone => "Halftone",
            Effect::Ascii => "ASCII",
        }
    }
}

/// The General and Appearance pages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// The CLI and composer choices a new thread starts with.
    pub provider: Provider,
    pub settings: TurnSettings,
    /// Enter steers a running turn instead of queueing; Ctrl+Enter does the other.
    pub enter_steers: bool,
    pub theme: Theme,
    pub check_updates: bool,
    /// An image behind the new-thread screen; empty for none.
    pub wallpaper: String,
    /// See-through, blurred window background where the OS supports it.
    pub glass: bool,
    /// A texture baked into the wallpaper.
    pub effect: Effect,
    /// `#rrggbb`, or empty for the theme's own.
    pub accent: String,
    pub open_in: OpenIn,
    /// The side last shown, for `OpenIn::Last`.
    pub last_chat: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            provider: Provider::Claude,
            settings: TurnSettings::default(),
            enter_steers: false,
            theme: Theme::System,
            check_updates: true,
            wallpaper: String::new(),
            glass: true,
            effect: Effect::Scanlines,
            accent: String::new(),
            open_in: OpenIn::Code,
            last_chat: false,
        }
    }
}

/// Tokens one CLI reported over a period.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub provider: Provider,
    pub input: u64,
    pub output: u64,
    pub turns: u64,
    pub threads: u64,
}

/// Totals per CLI, and tokens per day as `(day start, tokens)`.
pub type UsageReport = (Vec<UsageRow>, Vec<(i64, u64)>);

/// Settings as the UI sees them. API keys never leave the engine; the UI only
/// learns whether one is stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub api_key_set: Vec<Provider>,
    pub use_api_key: Vec<Provider>,
    pub mcp_servers: Vec<McpServer>,
    pub max_sessions: usize,
    pub data_dir: PathBuf,
    /// Starred models, as `(provider, model id)`.
    pub favorites: Vec<(Provider, String)>,
    pub providers: Vec<(Provider, ProviderConfig)>,
    pub prefs: Preferences,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative to the thread's folder, with `/` separators.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FileContent {
    Markdown(String),
    /// Source or plain text, with the extension for highlighting.
    Text {
        text: String,
        ext: String,
    },
    Image(PathBuf),
    Html(PathBuf),
    Binary(PathBuf),
}

/// Requests go in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Sent on connect and after missing updates; answered with a Snapshot.
    Hello,
    CreateProject {
        name: String,
        folder: Option<PathBuf>,
    },
    UpdateProject {
        id: ProjectId,
        name: String,
        instructions: String,
    },
    DeleteProject {
        id: ProjectId,
    },
    CreateThread {
        project: Option<ProjectId>,
        provider: Provider,
    },
    /// Creates a thread and sends its first message in one go.
    StartThread {
        project: Option<ProjectId>,
        provider: Provider,
        text: String,
        settings: TurnSettings,
        chat: bool,
    },
    PinThread {
        id: ThreadId,
        on: bool,
    },
    ArchiveThread {
        id: ThreadId,
        on: bool,
    },
    RenameThread {
        id: ThreadId,
        title: String,
    },
    DeleteThread {
        id: ThreadId,
    },
    OpenThread {
        id: ThreadId,
    },
    LoadOlder {
        id: ThreadId,
        before: Seq,
    },
    Send {
        thread: ThreadId,
        text: String,
        settings: TurnSettings,
        delivery: Delivery,
    },
    /// Remembers the composer's choices without sending anything.
    SetThreadSettings {
        thread: ThreadId,
        settings: TurnSettings,
    },
    /// Moves a thread that has no messages yet to another CLI.
    SetThreadProvider {
        thread: ThreadId,
        provider: Provider,
    },
    ListModels {
        provider: Provider,
    },
    ToggleFavorite {
        provider: Provider,
        model: String,
    },
    SetMaxSessions {
        count: usize,
    },
    SetProviderConfig {
        provider: Provider,
        config: ProviderConfig,
    },
    SetPreferences(Preferences),
    /// Token totals since a Unix time.
    Usage {
        since: i64,
    },
    /// The folders inside `path` (home when empty), for the add-project browser.
    ListDir {
        path: String,
    },
    Interrupt {
        thread: ThreadId,
    },
    /// Drops a queued message.
    Unqueue {
        thread: ThreadId,
        index: usize,
    },
    /// Folds a queued message into the running turn now.
    SteerQueued {
        thread: ThreadId,
        index: usize,
    },
    Resolve {
        thread: ThreadId,
        req_id: String,
        choice: PermChoice,
    },
    Answer {
        thread: ThreadId,
        req_id: String,
        answers: Vec<Vec<String>>,
    },
    /// Put the thread's files back to how they were before `turn`.
    Restore {
        thread: ThreadId,
        turn: u32,
    },
    Diff {
        thread: ThreadId,
        turn: u32,
    },
    ReadBlob {
        blob: BlobRef,
    },
    ListFiles {
        thread: ThreadId,
    },
    ReadFile {
        thread: ThreadId,
        path: String,
    },
    SetMemory {
        text: String,
    },
    SetApiKey {
        provider: Provider,
        key: Option<String>,
    },
    SetUseApiKey {
        provider: Provider,
        on: bool,
    },
    /// The MCP server list as JSON (an array of [`McpServer`]).
    SetMcpServers {
        json: String,
    },
    CheckAuth,
    SignIn {
        provider: Provider,
    },
    CreateTask {
        project: Option<ProjectId>,
        provider: Provider,
        prompt: String,
        every_minutes: Option<u32>,
    },
    DeleteTask {
        id: TaskId,
    },
    RunTask {
        id: TaskId,
    },
}

/// Updates come out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Update {
    Snapshot {
        projects: Vec<ProjectInfo>,
        threads: Vec<ThreadInfo>,
        tasks: Vec<TaskInfo>,
        auth: Vec<AuthStatus>,
        settings: SettingsView,
        memory: String,
    },
    Projects(Vec<ProjectInfo>),
    Threads(Vec<ThreadInfo>),
    Tasks(Vec<TaskInfo>),
    Auth(Vec<AuthStatus>),
    Settings(SettingsView),
    Memory(String),
    /// Events of a thread, oldest first. `prepend` pages go above what is
    /// loaded; otherwise they replace it. `turn` is the turn count before the
    /// first event, and `older` is where the next older page ends.
    Page {
        thread: ThreadId,
        events: Vec<ThreadEvent>,
        turn: u32,
        prepend: bool,
        older: Option<Seq>,
    },
    /// A live event of a thread.
    Event {
        thread: ThreadId,
        event: ThreadEvent,
    },
    Blob {
        blob: BlobRef,
        text: String,
    },
    Files {
        thread: ThreadId,
        files: Vec<FileEntry>,
    },
    File {
        thread: ThreadId,
        path: String,
        content: FileContent,
    },
    Diff {
        thread: ThreadId,
        turn: u32,
        text: String,
    },
    Notice {
        message: String,
        error: bool,
    },
    OpenUrl {
        url: String,
    },
    Models {
        provider: Provider,
        models: Vec<ModelInfo>,
    },
    /// Totals per CLI, and per day as `(day start, tokens)`.
    Usage {
        rows: Vec<UsageRow>,
        daily: Vec<(i64, u64)>,
    },
    /// `path` as resolved, and the names of the folders in it.
    Dir {
        path: String,
        dirs: Vec<String>,
    },
    /// What `/` offers in a provider's composer.
    Commands {
        provider: Provider,
        names: Vec<String>,
    },
    /// A CLI's subscription usage limits.
    Limits {
        provider: Provider,
        windows: Vec<LimitWindow>,
    },
    /// A newer release exists; `url` is its download page.
    UpdateAvailable {
        version: String,
        url: String,
    },
}

/// A wait such as "3h 10m", "45m" or "2d 4h".
pub fn wait_text(secs: i64) -> String {
    let minutes = (secs.max(0) + 59) / 60;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, mins) {
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(event: AgentEvent) -> ThreadEvent {
        ThreadEvent::Agent(event)
    }

    #[test]
    fn projection_merges_deltas_and_updates_rows_in_place() {
        let mut t = Transcript::default();
        let text = |s: &str| {
            agent(AgentEvent::TextDelta {
                msg_id: "m1".into(),
                text: s.into(),
            })
        };
        let todo = |status| {
            agent(AgentEvent::Plan {
                items: vec![TodoItem {
                    text: "a".into(),
                    status,
                }],
            })
        };
        let events = [
            ThreadEvent::User {
                text: "hi".into(),
                steer: false,
            },
            text("Hel"),
            text("lo"),
            agent(AgentEvent::ToolCall {
                call_id: "t1".into(),
                kind: ToolKind::Execute,
                title: "ls".into(),
                detail: String::new(),
            }),
            todo(TodoStatus::Pending),
            agent(AgentEvent::ToolUpdate {
                call_id: "t1".into(),
                status: ToolStatus::Done,
                preview: "x".into(),
                output: None,
            }),
            todo(TodoStatus::Done),
            agent(AgentEvent::PermissionRequest {
                req_id: "p".into(),
                title: "Edit".into(),
                detail: String::new(),
                plan: false,
            }),
            ThreadEvent::PermissionResolved {
                req_id: "p".into(),
                choice: PermChoice::Deny,
            },
            agent(AgentEvent::Usage {
                input: 3,
                output: 4,
            }),
            agent(AgentEvent::TurnEnded {
                reason: StopReason::EndTurn,
            }),
        ];
        for event in &events {
            t.apply(event);
        }
        assert_eq!(t.items.len(), 6, "{:#?}", t.items);
        assert_eq!(
            t.items[1],
            Item::Assistant {
                msg_id: "m1".into(),
                text: "Hello".into()
            }
        );
        assert!(
            matches!(&t.items[2], Item::Tool { status: ToolStatus::Done, preview, .. } if preview == "x")
        );
        assert!(matches!(&t.items[3], Item::Todo { items } if items[0].status == TodoStatus::Done));
        assert!(matches!(
            &t.items[4],
            Item::Permission {
                resolved: Some(PermChoice::Deny),
                ..
            }
        ));
        assert_eq!(
            t.items[5],
            Item::TurnEnd {
                turn: 1,
                reason: StopReason::EndTurn,
                input: 3,
                output: 4
            }
        );
    }

    #[test]
    fn steering_does_not_start_a_turn() {
        let mut t = Transcript::default();
        t.apply(&ThreadEvent::User {
            text: "a".into(),
            steer: false,
        });
        t.apply(&ThreadEvent::User {
            text: "b".into(),
            steer: true,
        });
        assert_eq!(t.turn, 1);
    }
}

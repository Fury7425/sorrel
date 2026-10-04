//! The window: a title bar, a sidebar of sessions (Code) or chats (Chat),
//! the open conversation or the new-thread screen, and settings pages, all
//! over an optional wallpaper. It renders [`Update`]s and sends
//! [`Request`]s; it never sees a vendor's wire format, spawns a process or
//! touches the database.

mod effects;
mod settings;
mod sidebar;
mod style;
mod thread;

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, Theme, ThemeMode,
    TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    AuthState, AuthStatus, LimitWindow, ModelInfo, OpenIn, Preferences, ProjectId, ProjectInfo, Provider, Request,
    SettingsView, TaskInfo, ThreadId, ThreadInfo, TurnSettings, Update, UsageReport,
};
use tokio::sync::mpsc;

pub use thread::ThreadView;

use style::{Backdrop, SIDEBAR, sidebar_spring};
use thread::Look;

actions!(sorrel, [NewThread, FocusSearch]);

/// Notices kept on screen; older ones drop off.
const MAX_NOTICES: usize = 3;

/// A client's link to the engine, in-process or over the daemon socket.
pub struct Connection {
    pub requests: mpsc::Sender<Request>,
    pub updates: mpsc::Receiver<Update>,
}

/// The screens a perf run visits.
#[derive(Clone, Copy, Debug)]
pub enum Screen {
    Home,
    Thread,
    Project,
    Tasks,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    General,
    Appearance,
    Providers,
    Connectors,
    Tasks,
    Archived,
    About,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    /// The new-thread (Code) or new-chat (Chat) screen.
    Home,
    Thread(ThreadId),
    Project(ProjectId),
    Settings(Section),
    Usage,
}

/// The add-project dialog's steps.
#[derive(Clone, Copy, PartialEq)]
enum Palette {
    Sources,
    Browse,
    Create,
}

struct Editors {
    memory: Entity<TextareaState>,
    mcp: Entity<TextareaState>,
    keys: Vec<(Provider, Entity<InputState>)>,
    binaries: Vec<(Provider, Entity<InputState>)>,
    args: Vec<(Provider, Entity<InputState>)>,
    env_key: Entity<InputState>,
    env_value: Entity<InputState>,
    task_prompt: Entity<TextareaState>,
    task_every: Entity<InputState>,
    project_name: Entity<InputState>,
    instructions: Entity<TextareaState>,
    search: Entity<InputState>,
    rename: Entity<InputState>,
    path: Entity<InputState>,
    new_project: Entity<InputState>,
}

pub struct Workspace {
    requests: mpsc::Sender<Request>,
    /// Holds keyboard focus when no field does, so shortcuts still reach us.
    focus: FocusHandle,
    projects: Vec<ProjectInfo>,
    threads: Vec<ThreadInfo>,
    tasks: Vec<TaskInfo>,
    auth: Vec<AuthStatus>,
    settings: SettingsView,
    catalog: HashMap<Provider, Vec<ModelInfo>>,
    commands: HashMap<Provider, Vec<String>>,
    /// Each CLI's subscription usage limits, as last reported.
    limits: HashMap<Provider, Vec<LimitWindow>>,
    /// Chat side rather than Code side.
    chat: bool,
    /// The opening side was chosen from the preferences.
    started: bool,
    view: View,
    thread: Option<Entity<ThreadView>>,
    code_draft: Entity<ThreadView>,
    chat_draft: Entity<ThreadView>,
    project_filter: Option<ProjectId>,
    filter_open: bool,
    /// The sidebar is shown; the title bar's panel button hides it.
    sidebar_open: bool,
    /// Views visited, for back and forward, and where we are in them.
    history: Vec<View>,
    cursor: usize,
    /// The wallpaper with its effect baked in: source, effect, baked file.
    baked: Option<(String, proto::Effect, std::path::PathBuf)>,
    baking: bool,
    /// The settings dropdown that is open, by id.
    open_select: Option<&'static str>,
    /// The Providers page shows the new-variable row.
    env_adding: bool,
    palette: Option<Palette>,
    dir: (String, Vec<String>),
    /// The provider open on the Providers page.
    /// The open card on the Providers page.
    provider_open: Option<Provider>,
    task_project: Option<ProjectId>,
    task_provider: Provider,
    usage: Option<UsageReport>,
    usage_days: i64,
    renaming: Option<ThreadId>,
    notices: VecDeque<(SharedString, bool)>,
    update: Option<(SharedString, String)>,
    editors: Editors,
    /// The preferences last painted onto the theme and window.
    applied: Option<Preferences>,
    _pump: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(connection: Connection, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Connection {
            requests,
            mut updates,
        } = connection;
        let _ = requests.try_send(Request::Hello);
        // Drain everything that arrived since the last wake-up, then render once.
        let _pump = cx.spawn_in(window, async move |this, cx| {
            while let Some(update) = updates.recv().await {
                let mut batch = vec![update];
                while let Ok(update) = updates.try_recv() {
                    batch.push(update);
                }
                let applied = this.update_in(cx, |this, window, cx| {
                    for update in batch {
                        this.apply(update, window, cx);
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        });

        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let area = |rows: usize,
                    placeholder: &'static str,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(rows)
                    .placeholder(placeholder)
            })
        };
        let per_provider = |placeholder: &'static str,
                            masked: bool,
                            window: &mut Window,
                            cx: &mut Context<Self>| {
            Provider::ALL
                .iter()
                .map(|&provider| {
                    let state = cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(masked)
                            .placeholder(placeholder)
                    });
                    (provider, state)
                })
                .collect::<Vec<_>>()
        };
        let editors = Editors {
            memory: area(5, "Things every session should know about you", window, cx),
            mcp: area(
                8,
                r#"[{"name": "files", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]}]"#,
                window,
                cx,
            ),
            keys: per_provider("API key", true, window, cx),
            binaries: per_provider("Found on PATH", false, window, cx),
            args: per_provider("--add-dir ../shared", false, window, cx),
            env_key: input("NAME", window, cx),
            env_value: input("value", window, cx),
            task_prompt: area(3, "What should the task do?", window, cx),
            task_every: input("Every N minutes (blank runs once)", window, cx),
            project_name: input("Project name", window, cx),
            instructions: area(
                10,
                "Instructions every session in this project follows",
                window,
                cx,
            ),
            search: input("Search", window, cx),
            rename: input("Title", window, cx),
            path: input("~/", window, cx),
            new_project: input("Project name", window, cx),
        };
        let mut _subscriptions = Vec::new();
        // Provider fields save on Enter (keys) and on Enter or leaving the field (runtime).
        for (provider, state) in &editors.keys {
            let provider = *provider;
            _subscriptions.push(cx.subscribe_in(
                state,
                window,
                move |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.save_key(provider, window, cx);
                    }
                },
            ));
        }
        for (provider, state) in editors.binaries.iter().chain(&editors.args) {
            let provider = *provider;
            _subscriptions.push(cx.subscribe(state, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.save_runtime(provider, cx);
                }
            }));
        }
        _subscriptions.extend([
            cx.subscribe(&editors.search, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&editors.rename, window, |this, _, event, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.commit_rename(cx);
                }
            }),
            cx.subscribe_in(&editors.path, window, |this, state, event, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let path = state.read(cx).value().to_string();
                    this.request(Request::ListDir { path });
                }
            }),
        ]);

        let code_draft = cx.new(|cx| {
            ThreadView::draft(
                false,
                Provider::Claude,
                TurnSettings::default(),
                requests.clone(),
                window,
                cx,
            )
        });
        let chat_draft = cx.new(|cx| {
            ThreadView::draft(
                true,
                Provider::Claude,
                TurnSettings::default(),
                requests.clone(),
                window,
                cx,
            )
        });

        cx.bind_keys([
            KeyBinding::new("secondary-n", NewThread, None),
            KeyBinding::new("secondary-k", FocusSearch, None),
        ]);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        Self {
            requests,
            focus,
            projects: Vec::new(),
            threads: Vec::new(),
            tasks: Vec::new(),
            auth: Vec::new(),
            settings: SettingsView::default(),
            catalog: HashMap::new(),
            commands: HashMap::new(),
            limits: HashMap::new(),
            chat: false,
            started: false,
            view: View::Home,
            thread: None,
            code_draft,
            chat_draft,
            project_filter: None,
            filter_open: false,
            open_select: None,
            sidebar_open: true,
            history: Vec::new(),
            cursor: 0,
            baked: None,
            baking: false,
            env_adding: false,
            palette: None,
            dir: (String::new(), Vec::new()),
            provider_open: Some(Provider::Claude),
            task_project: None,
            task_provider: Provider::Claude,
            usage: None,
            usage_days: 7,
            renaming: None,
            notices: VecDeque::new(),
            update: None,
            editors,
            applied: None,
            _pump,
            _subscriptions,
        }
    }

    /// Shows a screen as if its sidebar entry were clicked (perf runs).
    pub fn show(&mut self, screen: Screen, window: &mut Window, cx: &mut Context<Self>) {
        match screen {
            Screen::Home => self.view = View::Home,
            Screen::Tasks => self.view = View::Settings(Section::Tasks),
            Screen::Settings => self.view = View::Settings(Section::Providers),
            Screen::Project => {
                if let Some(id) = self.projects.first().map(|p| p.id) {
                    self.open_project(id, window, cx);
                }
            }
            Screen::Thread => {
                if let Some(id) = self.thread.as_ref().map(|t| t.read(cx).id) {
                    self.show_thread(id, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// The open thread's view, for the perf run.
    pub fn thread_view(&self) -> Option<Entity<ThreadView>> {
        self.thread.clone()
    }

    fn request(&self, request: Request) {
        let _ = self.requests.try_send(request);
    }

    fn notice(&mut self, message: impl Into<SharedString>, error: bool) {
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back((message.into(), error));
    }

    fn prefs(&self) -> &Preferences {
        &self.settings.prefs
    }

    fn set_prefs(&mut self, edit: impl FnOnce(&mut Preferences), cx: &mut Context<Self>) {
        let mut prefs = self.settings.prefs.clone();
        edit(&mut prefs);
        self.settings.prefs = prefs.clone();
        self.request(Request::SetPreferences(prefs));
        cx.notify();
    }

    fn apply(&mut self, update: Update, window: &mut Window, cx: &mut Context<Self>) {
        let current = match self.view {
            View::Thread(id) => Some(id),
            _ => None,
        };
        let thread = self.thread.clone();
        match update {
            Update::Snapshot {
                projects,
                threads,
                tasks,
                auth,
                settings,
                memory,
            } => {
                self.projects = projects;
                self.threads = threads;
                self.tasks = tasks;
                self.auth = auth;
                let servers =
                    serde_json::to_string_pretty(&settings.mcp_servers).unwrap_or_default();
                self.editors
                    .memory
                    .update(cx, |e, cx| e.set_value(memory, window, cx));
                self.editors
                    .mcp
                    .update(cx, |e, cx| e.set_value(servers, window, cx));
                self.load_provider_editors(&settings, window, cx);
                self.settings = settings;
                self.start(window, cx);
                self.apply_prefs(window, cx);
                self.sync_views(cx);
            }
            Update::Projects(projects) => {
                self.projects = projects;
                self.sync_views(cx);
            }
            Update::Threads(threads) => {
                self.threads = threads;
                self.sync_views(cx);
            }
            Update::Tasks(tasks) => self.tasks = tasks,
            Update::Auth(auth) => {
                self.auth = auth;
                self.sync_views(cx);
            }
            Update::Settings(settings) => {
                self.load_provider_editors(&settings, window, cx);
                self.settings = settings;
                self.apply_prefs(window, cx);
                self.sync_views(cx);
            }
            Update::Models { provider, models } => {
                self.catalog.insert(provider, models);
                self.sync_views(cx);
            }
            Update::Commands { provider, names } => {
                self.commands.insert(provider, names);
                self.sync_views(cx);
            }
            Update::Limits { provider, windows } => {
                self.limits.insert(provider, windows);
                self.sync_views(cx);
            }
            Update::Memory(_) => {}
            Update::Page {
                thread: id,
                events,
                turn,
                prepend,
                older,
            } => {
                if !prepend && current != Some(id) {
                    self.show_thread(id, window, cx);
                }
                if let (Some(view), true) = (&self.thread, self.view == View::Thread(id)) {
                    view.update(cx, |view, cx| {
                        view.apply_page(events, turn, prepend, older, cx)
                    });
                }
            }
            Update::Event { thread: id, event } => {
                if let (Some(view), true) = (thread, current == Some(id)) {
                    view.update(cx, |view, cx| view.apply_event(event, cx));
                }
            }
            Update::Blob { blob, text } => {
                if let Some(view) = thread {
                    view.update(cx, |view, cx| view.show_blob(&blob, text, cx));
                }
            }
            Update::Files { thread: id, files } => {
                if let (Some(view), true) = (thread, current == Some(id)) {
                    view.update(cx, |view, cx| view.show_files(files, cx));
                }
            }
            Update::File {
                thread: id,
                path,
                content,
            } => {
                if let (Some(view), true) = (thread, current == Some(id)) {
                    view.update(cx, |view, cx| view.show_file(path, content, cx));
                }
            }
            Update::Diff {
                thread: id,
                turn,
                text,
            } => {
                if let (Some(view), true) = (thread, current == Some(id)) {
                    view.update(cx, |view, cx| view.show_diff(turn, text, cx));
                }
            }
            Update::Usage { rows, daily } => self.usage = Some((rows, daily)),
            Update::Dir { path, dirs } => {
                self.editors
                    .path
                    .update(cx, |e, cx| e.set_value(path.clone(), window, cx));
                self.dir = (path, dirs);
            }
            Update::Notice { message, error } => self.notice(message, error),
            Update::OpenUrl { url } => cx.open_url(&url),
            Update::UpdateAvailable { version, url } => {
                if self.prefs().check_updates {
                    self.update = Some((format!("Sorrel {version} is available").into(), url));
                }
            }
        }
    }

    /// The first snapshot decides which side opens and what drafts start with.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.started {
            return;
        }
        self.started = true;
        let prefs = self.prefs().clone();
        // Model names for the chips; Claude's list is static, so this is cheap.
        self.request(Request::ListModels {
            provider: Provider::Claude,
        });
        if prefs.provider != Provider::Claude {
            self.request(Request::ListModels {
                provider: prefs.provider,
            });
        }
        self.chat = match prefs.open_in {
            OpenIn::Chat => true,
            OpenIn::Code => false,
            OpenIn::Last => prefs.last_chat,
        };
        for draft in [self.code_draft.clone(), self.chat_draft.clone()] {
            draft.update(cx, |view, cx| {
                view.reset_draft(prefs.provider, prefs.settings.clone(), window, cx)
            });
        }
    }

    /// Paints theme, accent and glass when they change.
    fn apply_prefs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.prefs().clone();
        if self.applied.as_ref() == Some(&prefs) {
            return;
        }
        let mode = match prefs.theme {
            proto::Theme::Light => ThemeMode::Light,
            proto::Theme::Dark => ThemeMode::Dark,
            proto::Theme::System => window.appearance().into(),
        };
        Theme::change(mode, Some(window), cx);
        style::apply_accent(&prefs.accent, cx);
        window.set_background_appearance(if prefs.glass {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Opaque
        });
        self.applied = Some(prefs);
    }

    fn load_provider_editors(
        &mut self,
        settings: &SettingsView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (provider, config) in &settings.providers {
            let changed = self
                .settings
                .providers
                .iter()
                .find(|(p, _)| p == provider)
                .is_none_or(|(_, old)| old != config);
            if !changed {
                continue;
            }
            for (list, value) in [
                (&self.editors.binaries, config.binary.clone()),
                (&self.editors.args, config.args.clone()),
            ] {
                if let Some((_, state)) = list.iter().find(|(p, _)| p == provider) {
                    state.update(cx, |e, cx| e.set_value(value, window, cx));
                }
            }
        }
    }

    fn look(&self) -> Look {
        let prefs = self.prefs();
        Look {
            wallpaper: !prefs.wallpaper.is_empty(),
            projects: self
                .projects
                .iter()
                .map(|p| (p.id, p.name.clone()))
                .collect(),
            enter_steers: prefs.enter_steers,
            limits: self.limits.clone(),
            enabled: self
                .settings
                .providers
                .iter()
                .filter(|(p, c)| {
                    !c.disabled
                        && !self
                            .auth
                            .iter()
                            .any(|a| a.provider == *p && a.state == AuthState::Missing)
                })
                .map(|(p, _)| *p)
                .collect(),
        }
    }

    /// Hands every conversation view its info, catalog and surroundings.
    fn sync_views(&mut self, cx: &mut Context<Self>) {
        let look = self.look();
        let (catalog, favorites, commands) = (
            self.catalog.clone(),
            self.settings.favorites.clone(),
            self.commands.clone(),
        );
        if let Some(view) = &self.thread {
            let id = view.read(cx).id;
            let info = self.threads.iter().find(|t| t.id == id).cloned();
            view.update(cx, |view, cx| view.set_info(info, cx));
        }
        let views = [
            Some(self.code_draft.clone()),
            Some(self.chat_draft.clone()),
            self.thread.clone(),
        ];
        for view in views.into_iter().flatten() {
            let (catalog, favorites, commands, look) = (
                catalog.clone(),
                favorites.clone(),
                commands.clone(),
                look.clone(),
            );
            view.update(cx, |view, cx| {
                view.set_catalog(catalog, favorites, commands, cx);
                view.set_look(look, cx);
            });
        }
    }

    fn show_thread(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let same = self
            .thread
            .as_ref()
            .is_some_and(|view| view.read(cx).id == id);
        if !same {
            let info = self.threads.iter().find(|t| t.id == id).cloned();
            let requests = self.requests.clone();
            self.thread = Some(cx.new(|cx| ThreadView::new(id, info, requests, window, cx)));
            self.sync_views(cx);
        }
        if let Some(info) = self.threads.iter().find(|t| t.id == id) {
            self.chat = info.chat;
        }
        self.view = View::Thread(id);
        cx.notify();
    }

    fn open_thread(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.show_thread(id, window, cx);
        self.request(Request::OpenThread { id });
    }

    fn open_project(&mut self, id: ProjectId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self.projects.iter().find(|p| p.id == id).cloned() {
            self.editors
                .project_name
                .update(cx, |e, cx| e.set_value(project.name, window, cx));
            self.editors
                .instructions
                .update(cx, |e, cx| e.set_value(project.instructions, window, cx));
        }
        self.view = View::Project(id);
        cx.notify();
    }

    /// The new-thread screen, optionally in a project.
    fn new_thread(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        self.view = View::Home;
        if !self.chat {
            self.code_draft
                .update(cx, |view, cx| view.set_project(project, cx));
        }
        cx.notify();
    }

    fn switch_side(&mut self, chat: bool, cx: &mut Context<Self>) {
        if self.chat == chat {
            return;
        }
        self.chat = chat;
        self.view = View::Home;
        self.set_prefs(|prefs| prefs.last_chat = chat, cx);
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.renaming.take() {
            let title = self.editors.rename.read(cx).value().trim().to_string();
            if !title.is_empty() {
                self.request(Request::RenameThread { id, title });
            }
            cx.notify();
        }
    }

    fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette = Some(Palette::Sources);
        cx.notify();
    }

    /// Adds `folder` as a project and closes the dialog.
    fn add_project(&mut self, folder: std::path::PathBuf, cx: &mut Context<Self>) {
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Project".into());
        self.request(Request::CreateProject {
            name,
            folder: Some(folder),
        });
        self.palette = None;
        cx.notify();
    }

    /// Asks the OS for a folder, then adds it.
    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(folder) = paths.pop()
            {
                let _ = this.update(cx, |this, cx| this.add_project(folder, cx));
            }
        })
        .detach();
    }

    /// Asks the OS for an image, then makes it the wallpaper.
    fn pick_wallpaper(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a wallpaper".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(file) = paths.pop()
            {
                let _ = this.update(cx, |this, cx| {
                    this.set_prefs(
                        |prefs| prefs.wallpaper = file.to_string_lossy().into_owned(),
                        cx,
                    )
                });
            }
        })
        .detach();
    }

    /// Notes the current view as a step in the history, dropping anything
    /// ahead of it. Called each render, so every way of changing view counts.
    fn record_history(&mut self) {
        const MAX_HISTORY: usize = 50;
        if self.history.get(self.cursor) == Some(&self.view) {
            return;
        }
        self.history.truncate(self.cursor + 1);
        self.history.push(self.view);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        self.cursor = self.history.len() - 1;
    }

    /// Steps back (`-1`) or forward (`1`) through the history.
    fn step(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cursor) = self
            .cursor
            .checked_add_signed(by)
            .filter(|&c| c < self.history.len())
        else {
            return;
        };
        self.cursor = cursor;
        match self.history[cursor] {
            View::Thread(id) if self.threads.iter().any(|t| t.id == id) => {
                self.open_thread(id, window, cx)
            }
            View::Thread(_) => self.view = View::Home,
            View::Project(id) => self.open_project(id, window, cx),
            view => self.view = view,
        }
        // A view that no longer exists was replaced; keep the cursor on it.
        self.history[cursor] = self.view;
        cx.notify();
    }

    /// The wallpaper file to draw: the baked one when its effect is ready,
    /// else the original while a bake runs in the background.
    fn wallpaper_file(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let prefs = self.prefs();
        if prefs.wallpaper.is_empty() {
            return None;
        }
        let (source, effect) = (prefs.wallpaper.clone(), prefs.effect);
        // Scanlines are drawn live over the picture; the rest are baked into it.
        if matches!(effect, proto::Effect::None | proto::Effect::Scanlines) {
            return Some(source);
        }
        if let Some((s, e, file)) = &self.baked
            && *s == source
            && *e == effect
        {
            return Some(file.to_string_lossy().into_owned());
        }
        if !self.baking {
            self.baking = true;
            let out = effects::cache_path(&self.settings.data_dir.join("cache"), &source, effect);
            let input = std::path::PathBuf::from(&source);
            let task = cx
                .background_spawn(async move { effects::bake(&input, effect, &out).map(|_| out) });
            let key = source.clone();
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.baking = false;
                    match result {
                        Ok(file) => this.baked = Some((key, effect, file)),
                        Err(message) => this.notice(message, true),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        Some(source)
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, primary, warning) = (theme.muted_foreground, theme.primary, theme.warning);
        let crumb = |parent: Option<String>, title: String| {
            h_flex()
                .gap_1p5()
                .text_sm()
                .when_some(parent, |el, parent| {
                    el.child(div().text_color(muted).child(parent))
                        .child(div().text_color(muted.opacity(0.6)).child("/"))
                })
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .truncate()
                        .child(title),
                )
        };
        let project_name = |id: Option<ProjectId>| {
            id.and_then(|id| self.projects.iter().find(|p| p.id == id))
                .map(|p| p.name.clone())
        };
        let (left, status, actions) = match self.view {
            View::Thread(id) => {
                let info = self.threads.iter().find(|t| t.id == id);
                let title = info.map(|i| i.title.clone()).unwrap_or_default();
                let parent = info.and_then(|i| project_name(i.project));
                let status = info.and_then(|i| {
                    if i.needs_input {
                        Some(("Needs approval", warning))
                    } else if i.running {
                        Some(("Working", primary))
                    } else {
                        None
                    }
                });
                let folder = info
                    .filter(|i| i.project.is_some())
                    .map(|i| i.folder.clone());
                let pane_open = self.thread.as_ref().is_some_and(|t| t.read(cx).pane_open());
                let actions = h_flex()
                    .occlude()
                    .gap_1()
                    .when_some(folder, |el, folder| {
                        el.child(
                            Button::new("open-folder")
                                .ghost()
                                .xsmall()
                                .icon(IconName::FolderOpen)
                                .label("Open folder")
                                .on_click(move |_, _, cx| cx.open_url(&thread::file_url(&folder))),
                        )
                    })
                    .when(!self.chat, |el| {
                        el.child(
                            Button::new("toggle-pane")
                                .ghost()
                                .xsmall()
                                .icon(IconName::PanelRight)
                                .selected(pane_open)
                                .tooltip("Files and changes")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(view) = &this.thread {
                                        view.update(cx, |view, cx| view.toggle_pane(cx));
                                    }
                                })),
                        )
                    });
                (crumb(parent, title), status, Some(actions))
            }
            View::Home if self.chat => (crumb(None, "New chat".into()), None, None),
            View::Home => {
                let parent = project_name(self.code_draft.read(cx).project());
                (crumb(parent, "New thread".into()), None, None)
            }
            View::Project(id) => (
                crumb(
                    Some("Project".into()),
                    project_name(Some(id)).unwrap_or_default(),
                ),
                None,
                None,
            ),
            View::Settings(_) => (crumb(None, "Settings".into()), None, None),
            View::Usage => (crumb(None, "Usage".into()), None, None),
        };
        let _ = window;
        let (can_back, can_forward) = (self.cursor > 0, self.cursor + 1 < self.history.len());
        // Buttons in the title bar block the drag strip under them; otherwise
        // Windows reads every click there as the start of a window move.
        let nav = h_flex()
            .occlude()
            .gap_0p5()
            .flex_shrink_0()
            .with_spring("sidebar-nav", sidebar_spring(self.sidebar_open), |el, w| {
                el.min_w(w * ((SIDEBAR - 6.) / SIDEBAR))
            })
            .child(
                Button::new("toggle-sidebar")
                    .ghost()
                    .xsmall()
                    .icon(if self.sidebar_open {
                        IconName::PanelLeftClose
                    } else {
                        IconName::PanelLeftOpen
                    })
                    .tooltip(if self.sidebar_open {
                        "Hide sidebar"
                    } else {
                        "Show sidebar"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("history-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back")
                    .disabled(!can_back)
                    .on_click(cx.listener(|this, _, window, cx| this.step(-1, window, cx))),
            )
            .child(
                Button::new("history-forward")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowRight)
                    .tooltip("Forward")
                    .disabled(!can_forward)
                    .on_click(cx.listener(|this, _, window, cx| this.step(1, window, cx))),
            );
        TitleBar::new().bg(transparent_black()).border_b_0().child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_3()
                .child(nav)
                .child(left)
                .when_some(status, |el, (label, color)| {
                    el.child(
                        h_flex()
                            .gap_1p5()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(color.opacity(0.15))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color)
                            .child(div().size(px(6.)).rounded_full().bg(color))
                            .child(label),
                    )
                })
                .child(div().flex_1())
                .children(actions)
                .child(div().w(px(8.))),
        )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let step = self.palette?;
        let theme = cx.theme();
        let (fg, muted, popover) = (theme.foreground, theme.muted_foreground, theme.popover);
        let source = |id: &'static str, icon: IconName, title: &'static str, hint: &'static str| {
            h_flex()
                .id(id)
                .gap_3()
                .p_2()
                .rounded_lg()
                .cursor_pointer()
                .hover(move |style| style.bg(fg.opacity(0.06)))
                .child(gpui_kit::component::Icon::new(icon).text_color(muted))
                .child(
                    v_flex()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_xs().text_color(muted).child(hint)),
                )
        };
        let body = match step {
            Palette::Sources => v_flex()
                .gap_1()
                .p_2()
                .child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(muted)
                        .child("Add a project from"),
                )
                .child(
                    source(
                        "src-local",
                        IconName::FolderOpen,
                        "Local folder",
                        "Browse a folder on disk",
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.palette = Some(Palette::Browse);
                        let path = this.editors.path.read(cx).value().to_string();
                        this.request(Request::ListDir { path });
                        cx.notify();
                    })),
                )
                .child(
                    source(
                        "src-new",
                        IconName::Plus,
                        "New project",
                        "Start an empty folder in Sorrel's projects folder",
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.palette = Some(Palette::Create);
                        cx.notify();
                    })),
                )
                .into_any_element(),
            Palette::Create => v_flex()
                .gap_3()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("New project"),
                )
                .child(Input::new(&self.editors.new_project))
                .child(
                    h_flex().gap_2().child(div().flex_1()).child(
                        Button::new("create-project")
                            .primary()
                            .small()
                            .label("Create")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let name = this.editors.new_project.read(cx).value().to_string();
                                this.request(Request::CreateProject { name, folder: None });
                                this.editors
                                    .new_project
                                    .update(cx, |e, cx| e.set_value("", window, cx));
                                this.palette = None;
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element(),
            Palette::Browse => {
                let base = self.dir.0.clone();
                let dirs = v_flex()
                    .id("dir-list")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .gap_0p5()
                    .children(self.dir.1.iter().enumerate().map(|(ix, name)| {
                        let next = format!("{base}{name}{}", std::path::MAIN_SEPARATOR);
                        h_flex()
                            .id(("dir", ix))
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(move |style| style.bg(fg.opacity(0.06)))
                            .child(
                                gpui_kit::component::Icon::new(IconName::Folder)
                                    .small()
                                    .text_color(muted),
                            )
                            .child(div().text_sm().child(name.clone()))
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::ListDir { path: next.clone() })
                            }))
                    }));
                v_flex()
                    .gap_2()
                    .p_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("browse-back")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::ChevronLeft)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.palette = Some(Palette::Sources);
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1().child(Input::new(&self.editors.path).small()))
                            .child(
                                Button::new("browse-add")
                                    .primary()
                                    .small()
                                    .label("Add")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let path =
                                            this.editors.path.read(cx).value().trim().to_string();
                                        if !path.is_empty() {
                                            this.add_project(path.into(), cx);
                                        }
                                    })),
                            ),
                    )
                    .child(dirs)
                    .child(
                        h_flex()
                            .text_xs()
                            .text_color(muted)
                            .child("Enter opens the typed path. Click a folder to go in.")
                            .child(div().flex_1())
                            .child(
                                Button::new("system-picker")
                                    .ghost()
                                    .xsmall()
                                    .label("Open system picker")
                                    .on_click(cx.listener(|this, _, _, cx| this.pick_folder(cx))),
                            ),
                    )
                    .into_any_element()
            }
        };
        Some(
            div()
                .id("palette-overlay")
                .absolute()
                .inset_0()
                .bg(black().opacity(0.5))
                .flex()
                .items_start()
                .justify_center()
                .pt(px(90.))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.palette = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("palette")
                        .w(px(540.))
                        .h_auto()
                        .rounded_xl()
                        .bg(popover.opacity(0.96))
                        .border_1()
                        .border_color(fg.opacity(0.1))
                        .shadow_lg()
                        // Clicks inside stay inside.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn render_toasts(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (fg, danger, popover) = (theme.foreground, theme.danger, theme.popover);
        let pill = |content: Div| {
            content
                .gap_2()
                .pl_4()
                .pr_1()
                .py_1()
                .rounded_full()
                .bg(popover.opacity(0.92))
                .border_1()
                .border_color(fg.opacity(0.1))
                .shadow_lg()
                .text_sm()
        };
        // Toasts rise into place; dismissal is instant, as leaving should be.
        let rise = |id: ElementId, pill: Div| {
            pill.with_animation(
                id,
                Animation::new(Duration::from_millis(240)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t).mt(px(10. * (1. - t))),
            )
        };
        let notices = self
            .notices
            .iter()
            .enumerate()
            .map(|(ix, (message, error))| {
                let pill = pill(h_flex())
                    .when(*error, |el| el.text_color(danger))
                    .child(div().max_w(px(560.)).child(message.clone()))
                    .child(
                        Button::new(("dismiss", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.notices.remove(ix);
                                cx.notify();
                            })),
                    );
                rise(ElementId::NamedInteger("toast".into(), ix as u64), pill)
            });
        let update = self.update.clone().map(|(message, url)| {
            let pill = pill(h_flex())
                .child(message)
                .child(
                    Button::new("get-update")
                        .primary()
                        .xsmall()
                        .rounded_full()
                        .label("Download")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .child(
                    Button::new("dismiss-update")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.update = None;
                            cx.notify();
                        })),
                );
            rise("update-toast".into(), pill)
        });
        v_flex()
            .absolute()
            .bottom_5()
            .left_0()
            .right_0()
            .items_center()
            .gap_2()
            .children(update)
            .children(notices)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.record_history();
        let wallpaper_file = self.wallpaper_file(cx);
        let titlebar = self.render_titlebar(window, cx);
        let in_settings = matches!(self.view, View::Settings(_));
        let sidebar = if in_settings {
            self.render_settings_nav(cx).into_any_element()
        } else {
            self.render_sidebar(cx).into_any_element()
        };
        let main = match self.view {
            View::Home if self.chat => self.chat_draft.clone().into_any_element(),
            View::Home => self.code_draft.clone().into_any_element(),
            View::Thread(_) => match &self.thread {
                Some(view) => view.clone().into_any_element(),
                None => self.code_draft.clone().into_any_element(),
            },
            View::Project(id) => self.render_project(id, cx),
            View::Settings(section) => self.render_settings(section, cx),
            View::Usage => self.render_usage(cx),
        };
        let palette = self.render_palette(cx);
        let toasts = self.render_toasts(cx);
        let prefs = self.prefs().clone();
        let theme = cx.theme();
        let (background, foreground, sidebar_bg) =
            (theme.background, theme.foreground, theme.sidebar);
        let backdrop = match self.view {
            View::Home => Backdrop::Hero,
            View::Thread(_) => Backdrop::Session,
            _ => Backdrop::Quiet,
        };
        let wallpaper = wallpaper_file.is_some();
        // Over a busy wallpaper the sidebar needs more body to stay legible;
        // glass alone gets the OS blur behind it.
        let sidebar_fill = if wallpaper {
            sidebar_bg.opacity(0.8)
        } else if prefs.glass {
            sidebar_bg.opacity(0.58)
        } else {
            sidebar_bg
        };
        let open = self.sidebar_open;
        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NewThread, _, cx| {
                let project = if this.chat { None } else { this.project_filter };
                this.new_thread(project, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar_open = true;
                let search = this.editors.search.read(cx).focus_handle(cx);
                window.focus(&search, cx);
                cx.notify();
            }))
            .bg(if prefs.glass && !wallpaper {
                background.opacity(0.8)
            } else {
                background
            })
            .text_color(foreground)
            .when_some(wallpaper_file, |el, file| {
                el.child(style::wallpaper(
                    &file,
                    backdrop,
                    prefs.effect == proto::Effect::Scanlines,
                    background,
                ))
            })
            // The panel and its contents ride one spring, so a toggle mid-slide
            // turns around from where it is instead of jumping.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .bottom_0()
                    .bg(sidebar_fill)
                    .border_r_1()
                    .border_color(foreground.opacity(0.07))
                    .with_spring("sidebar-panel", sidebar_spring(open), |el, w| {
                        el.w(w).opacity((w / px(SIDEBAR)).clamp(0., 1.))
                    }),
            )
            .child(
                v_flex().relative().size_full().child(titlebar).child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .items_start()
                        .child(
                            div()
                                .h_full()
                                .flex_shrink_0()
                                .overflow_hidden()
                                .child(div().w(px(SIDEBAR)).h_full().child(sidebar))
                                .with_spring("sidebar-body", sidebar_spring(open), |el, w| {
                                    let shown = (w / px(SIDEBAR)).clamp(0., 1.);
                                    el.w(w).opacity(shown)
                                }),
                        )
                        .child(div().flex_1().min_w_0().h_full().child(main)),
                ),
            )
            .children(palette)
            .child(toasts)
    }
}

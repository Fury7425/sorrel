//! The window, laid out like T3 Code: a sidebar of threads grouped under
//! their projects, the open thread, and a settings page with its own nav.
//! It renders [`Update`]s and sends [`Request`]s; it never sees a vendor's
//! wire format, spawns a process or touches the database.

mod thread;

use std::collections::{HashMap, VecDeque};

use gpui_kit::component::{
    ActiveTheme as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    AuthState, AuthStatus, ModelInfo, ProjectId, ProjectInfo, Provider, Request, SettingsView,
    TaskInfo, ThreadId, ThreadInfo, Update,
};
use tokio::sync::mpsc;

pub use thread::ThreadView;

/// Notices kept on screen; older ones scroll off.
const MAX_NOTICES: usize = 4;

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
    Providers,
    Connectors,
    Tasks,
    About,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::General,
        Section::Providers,
        Section::Connectors,
        Section::Tasks,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Providers => "Providers",
            Section::Connectors => "Connectors",
            Section::Tasks => "Scheduled tasks",
            Section::About => "About",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Home,
    Thread(ThreadId),
    Project(ProjectId),
    Settings(Section),
}

struct Editors {
    new_project: Entity<InputState>,
    new_project_folder: Entity<InputState>,
    project_name: Entity<InputState>,
    instructions: Entity<TextareaState>,
    memory: Entity<TextareaState>,
    mcp: Entity<TextareaState>,
    keys: Vec<(Provider, Entity<InputState>)>,
    task_prompt: Entity<TextareaState>,
    task_every: Entity<InputState>,
}

pub struct Workspace {
    requests: mpsc::Sender<Request>,
    projects: Vec<ProjectInfo>,
    threads: Vec<ThreadInfo>,
    tasks: Vec<TaskInfo>,
    auth: Vec<AuthStatus>,
    settings: SettingsView,
    catalog: HashMap<Provider, Vec<ModelInfo>>,
    view: View,
    thread: Option<Entity<ThreadView>>,
    /// The CLI new threads start on; the thread's model picker can change it.
    provider: Provider,
    task_project: Option<ProjectId>,
    adding_project: bool,
    notices: VecDeque<(SharedString, bool)>,
    update: Option<(SharedString, String)>,
    editors: Editors,
    _pump: Task<()>,
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
        let editors = Editors {
            new_project: input("Project name", window, cx),
            new_project_folder: input("Existing folder (optional)", window, cx),
            project_name: input("Project name", window, cx),
            instructions: area(
                10,
                "Instructions every session in this project follows",
                window,
                cx,
            ),
            memory: area(8, "Things every session should know about you", window, cx),
            mcp: area(
                8,
                r#"[{"name": "files", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]}]"#,
                window,
                cx,
            ),
            keys: Provider::ALL
                .iter()
                .map(|&provider| {
                    let key = cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(true)
                            .placeholder("API key")
                    });
                    (provider, key)
                })
                .collect(),
            task_prompt: area(4, "What should the task do?", window, cx),
            task_every: input("Repeat every N minutes (blank runs once)", window, cx),
        };

        Self {
            requests,
            projects: Vec::new(),
            threads: Vec::new(),
            tasks: Vec::new(),
            auth: Vec::new(),
            settings: SettingsView::default(),
            catalog: HashMap::new(),
            view: View::Home,
            thread: None,
            provider: Provider::Claude,
            task_project: None,
            adding_project: false,
            notices: VecDeque::new(),
            update: None,
            editors,
            _pump,
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
                self.settings = settings;
                self.editors
                    .memory
                    .update(cx, |e, cx| e.set_value(memory, window, cx));
                self.editors
                    .mcp
                    .update(cx, |e, cx| e.set_value(servers, window, cx));
                self.sync_thread(cx);
            }
            Update::Projects(projects) => self.projects = projects,
            Update::Threads(threads) => {
                self.threads = threads;
                self.sync_thread(cx);
            }
            Update::Tasks(tasks) => self.tasks = tasks,
            Update::Auth(auth) => self.auth = auth,
            Update::Settings(settings) => {
                self.settings = settings;
                self.sync_thread(cx);
            }
            Update::Models { provider, models } => {
                self.catalog.insert(provider, models);
                self.sync_thread(cx);
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
            Update::Notice { message, error } => self.notice(message, error),
            Update::OpenUrl { url } => cx.open_url(&url),
            Update::UpdateAvailable { version, url } => {
                self.update = Some((format!("Sorrel {version} is available.").into(), url));
            }
        }
    }

    /// Hands the open thread its info, the model catalog and favorites.
    fn sync_thread(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.thread {
            let id = view.read(cx).id;
            let info = self.threads.iter().find(|t| t.id == id).cloned();
            let (catalog, favorites) = (self.catalog.clone(), self.settings.favorites.clone());
            view.update(cx, |view, cx| {
                view.set_info(info, cx);
                view.set_catalog(catalog, favorites, cx);
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
            self.sync_thread(cx);
        }
        self.view = View::Thread(id);
        cx.notify();
    }

    fn open_thread(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
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

    fn new_thread(&self, project: Option<ProjectId>) {
        self.request(Request::CreateThread {
            project,
            provider: self.provider,
        });
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (border, muted, sidebar) = (theme.border, theme.muted_foreground, theme.sidebar);
        let now = now();
        let thread_row = |thread: &ThreadInfo| {
            let id = thread.id;
            let status = if thread.needs_input {
                "Approval".to_owned()
            } else if thread.running {
                "Working".to_owned()
            } else {
                ago(now - thread.updated_at)
            };
            let selected = self.view == View::Thread(id);
            h_flex()
                .id(("thread", id as u64))
                .gap_2()
                .pl_6()
                .pr_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |el| el.bg(border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .truncate()
                        .child(thread.title.clone()),
                )
                .child(div().text_xs().text_color(muted).child(status))
                .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)))
        };
        let group = |label: String, project: Option<ProjectId>, key: usize| {
            h_flex()
                .gap_1()
                .pl_2()
                .pr_1()
                .pt_2()
                .child(
                    div()
                        .id(("group", key))
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(muted)
                        .truncate()
                        .cursor_pointer()
                        .child(label)
                        .when_some(project, |el, id| {
                            el.on_click(cx.listener(move |this, _, window, cx| {
                                this.open_project(id, window, cx)
                            }))
                        }),
                )
                .child(
                    Button::new(("group-new", key))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .on_click(cx.listener(move |this, _, _, _| this.new_thread(project))),
                )
        };

        let mut list = v_flex()
            .id("sidebar-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_0p5();
        for (ix, project) in self.projects.iter().enumerate() {
            list = list.child(group(project.name.clone(), Some(project.id), ix + 1));
            for thread in self
                .threads
                .iter()
                .filter(|t| t.project == Some(project.id))
            {
                list = list.child(thread_row(thread));
            }
        }
        list = list.child(group("Chats".into(), None, 0));
        for thread in self.threads.iter().filter(|t| t.project.is_none()) {
            list = list.child(thread_row(thread));
        }
        if self.adding_project {
            list = list.child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .child(Input::new(&self.editors.new_project).small())
                    .child(Input::new(&self.editors.new_project_folder).small())
                    .child(
                        Button::new("create-project")
                            .primary()
                            .xsmall()
                            .label("Create project")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let name = this.editors.new_project.read(cx).value().to_string();
                                let folder = this
                                    .editors
                                    .new_project_folder
                                    .read(cx)
                                    .value()
                                    .trim()
                                    .to_string();
                                this.request(Request::CreateProject {
                                    name,
                                    folder: (!folder.is_empty()).then(|| folder.into()),
                                });
                                this.editors
                                    .new_project
                                    .update(cx, |e, cx| e.set_value("", window, cx));
                                this.editors
                                    .new_project_folder
                                    .update(cx, |e, cx| e.set_value("", window, cx));
                                this.adding_project = false;
                                cx.notify();
                            })),
                    ),
            );
        }

        let in_settings = matches!(self.view, View::Settings(_));
        v_flex()
            .w(px(272.))
            .h_full()
            .flex_shrink_0()
            .gap_1()
            .p_2()
            .bg(sidebar)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Sorrel"),
                    )
                    .child(
                        Button::new("new-thread")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("New thread")
                            .on_click(cx.listener(|this, _, _, _| this.new_thread(None))),
                    ),
            )
            .child(list)
            .child(
                Button::new("add-project")
                    .ghost()
                    .small()
                    .w_full()
                    .icon(IconName::Folder)
                    .label("Add project")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.adding_project = !this.adding_project;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .small()
                    .w_full()
                    .icon(IconName::Settings)
                    .label("Settings")
                    .selected(in_settings)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::Settings(Section::General);
                        this.request(Request::CheckAuth);
                        cx.notify();
                    })),
            )
    }

    fn render_home(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("What should we work on?"))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Threads run the official CLIs you already sign in to. Pick the model inside the thread."),
            )
            .child(
                Button::new("home-new-thread")
                    .primary()
                    .icon(IconName::Plus)
                    .label("New thread")
                    .on_click(cx.listener(|this, _, _, _| this.new_thread(None))),
            )
            .into_any_element()
    }

    fn render_project(&self, id: ProjectId, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(project) = self.projects.iter().find(|p| p.id == id) else {
            return div()
                .p_6()
                .child("This project was removed.")
                .into_any_element();
        };
        let folder = project.folder.clone();
        page("Project")
            .child(Input::new(&self.editors.project_name))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Folder: {}", project.folder.display())),
            )
            .child(field_label(
                "Instructions",
                "Written to CLAUDE.md and AGENTS.md in the folder.",
                muted,
            ))
            .child(Textarea::new(&self.editors.instructions))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-project")
                            .primary()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let name = this.editors.project_name.read(cx).value().to_string();
                                let instructions =
                                    this.editors.instructions.read(cx).value().to_string();
                                this.request(Request::UpdateProject {
                                    id,
                                    name,
                                    instructions,
                                });
                            })),
                    )
                    .child(
                        Button::new("project-thread")
                            .small()
                            .icon(IconName::Plus)
                            .label("New thread")
                            .on_click(cx.listener(move |this, _, _, _| this.new_thread(Some(id)))),
                    )
                    .child(
                        Button::new("project-folder")
                            .ghost()
                            .small()
                            .icon(IconName::FolderOpen)
                            .label("Open folder")
                            .on_click(move |_, _, cx| cx.open_url(&thread::file_url(&folder))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("delete-project")
                            .danger()
                            .small()
                            .label("Remove project")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request(Request::DeleteProject { id });
                                this.view = View::Home;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_settings(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let nav = v_flex()
            .w(px(200.))
            .flex_shrink_0()
            .gap_0p5()
            .p_3()
            .border_r_1()
            .border_color(border)
            .children(Section::ALL.iter().enumerate().map(|(ix, &item)| {
                Button::new(("section", ix))
                    .ghost()
                    .small()
                    .w_full()
                    .label(item.label())
                    .selected(item == section)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.view = View::Settings(item);
                        cx.notify();
                    }))
            }));
        let content = match section {
            Section::General => self.render_general(cx),
            Section::Providers => self.render_providers(cx),
            Section::Connectors => self.render_connectors(cx),
            Section::Tasks => self.render_tasks(cx),
            Section::About => self.render_about(cx),
        };
        h_flex()
            .size_full()
            .items_start()
            .child(nav)
            .child(div().flex_1().min_w_0().h_full().child(content))
            .into_any_element()
    }

    fn render_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let max = self.settings.max_sessions;
        page("General")
            .child(field_label(
                "Memory",
                "Shared with every session through CLAUDE.md and AGENTS.md.",
                muted,
            ))
            .child(Textarea::new(&self.editors.memory))
            .child(
                h_flex().child(
                    Button::new("save-memory")
                        .small()
                        .label("Save memory")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let text = this.editors.memory.read(cx).value().to_string();
                            this.request(Request::SetMemory { text });
                        })),
                ),
            )
            .child(field_label(
                "Sessions at once",
                "Turns beyond this wait for a free slot.",
                muted,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("sessions-down")
                            .small()
                            .icon(IconName::Minus)
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::SetMaxSessions {
                                    count: max.saturating_sub(1),
                                })
                            })),
                    )
                    .child(div().text_sm().child(max.to_string()))
                    .child(
                        Button::new("sessions-up")
                            .small()
                            .icon(IconName::Plus)
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::SetMaxSessions { count: max + 1 })
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let cards = Provider::ALL.iter().enumerate().map(|(ix, &provider)| {
            let (state, detail) = self
                .auth
                .iter()
                .find(|a| a.provider == provider)
                .map(|s| (s.state, s.detail.clone()))
                .unwrap_or((AuthState::Unknown, String::new()));
            let state_label = match state {
                AuthState::Unknown => "Unknown",
                AuthState::Missing => "Not installed",
                AuthState::SignedOut => "Signed out",
                AuthState::Subscription => "Signed in",
                AuthState::ApiKey => "API key",
            };
            let key = self
                .editors
                .keys
                .iter()
                .find(|(p, _)| *p == provider)
                .map(|(_, k)| k.clone());
            let key_set = self.settings.api_key_set.contains(&provider);
            let using_key = self.settings.use_api_key.contains(&provider);
            v_flex()
                .gap_2()
                .p_3()
                .rounded_xl()
                .border_1()
                .border_color(border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(provider.label()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(
                                    if matches!(state, AuthState::Missing | AuthState::SignedOut) {
                                        danger
                                    } else {
                                        muted
                                    },
                                )
                                .child(state_label),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(detail),
                        )
                        .child(
                            Button::new(("sign-in", ix))
                                .xsmall()
                                .label("Sign in")
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.request(Request::SignIn { provider })
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .children(key.map(|key| div().flex_1().child(Input::new(&key).small())))
                        .child(
                            Button::new(("save-key", ix))
                                .xsmall()
                                .label("Save key")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some((_, key)) = this
                                        .editors
                                        .keys
                                        .iter()
                                        .find(|(p, _)| *p == provider)
                                        .cloned()
                                    {
                                        let value = key.read(cx).value().to_string();
                                        key.update(cx, |e, cx| e.set_value("", window, cx));
                                        this.request(Request::SetApiKey {
                                            provider,
                                            key: Some(value),
                                        });
                                    }
                                })),
                        )
                        .when(key_set, |el| {
                            el.child(
                                Button::new(("use-key", ix))
                                    .ghost()
                                    .xsmall()
                                    .label("Use API key")
                                    .selected(using_key)
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.request(Request::SetUseApiKey {
                                            provider,
                                            on: !using_key,
                                        })
                                    })),
                            )
                            .child(
                                Button::new(("remove-key", ix))
                                    .ghost()
                                    .xsmall()
                                    .label("Remove key")
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.request(Request::SetApiKey {
                                            provider,
                                            key: None,
                                        })
                                    })),
                            )
                        }),
                )
        });
        page("Providers")
            .child(div().text_sm().text_color(muted).child(
                "Sign-in happens inside each official CLI; Sorrel never sees those tokens. API keys you enter here stay on this machine.",
            ))
            .child(h_flex().child(Button::new("check-auth").xsmall().icon(IconName::RefreshCw).label("Check again").on_click(
                cx.listener(|this, _, _, _| this.request(Request::CheckAuth)),
            )))
            .children(cards)
            .into_any_element()
    }

    fn render_connectors(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        page("Connectors")
            .child(field_label(
                "MCP servers",
                "A JSON list, passed to every CLI when a session starts.",
                muted,
            ))
            .child(Textarea::new(&self.editors.mcp))
            .child(
                h_flex().child(
                    Button::new("save-mcp")
                        .small()
                        .label("Save connectors")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let json = this.editors.mcp.read(cx).value().to_string();
                            this.request(Request::SetMcpServers { json });
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_tasks(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let project_button = |label: String, project: Option<ProjectId>, ix: usize| {
            Button::new(("task-project", ix))
                .ghost()
                .xsmall()
                .label(label)
                .selected(self.task_project == project)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.task_project = project;
                    cx.notify();
                }))
        };
        let provider_button = |provider: Provider| {
            Button::new(("task-provider", provider as usize))
                .ghost()
                .xsmall()
                .label(provider.label())
                .selected(self.provider == provider)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.provider = provider;
                    cx.notify();
                }))
        };
        let now = now();
        let list = self.tasks.iter().map(|task| {
            let id = task.id;
            let schedule = match task.every_minutes {
                Some(minutes) => format!("every {minutes} min"),
                None => "once".into(),
            };
            let next = if task.next_run < 0 {
                "done".to_owned()
            } else if task.next_run <= now {
                "due now".to_owned()
            } else {
                format!("next in {} min", (task.next_run - now + 59) / 60)
            };
            let thread = task.thread;
            v_flex()
                .gap_1()
                .p_3()
                .rounded_xl()
                .border_1()
                .border_color(border)
                .child(
                    div()
                        .text_sm()
                        .child(task.prompt.chars().take(200).collect::<String>()),
                )
                .child(div().text_xs().text_color(muted).child(format!(
                    "{} · {schedule} · {next} · {}",
                    task.provider.label(),
                    if task.last_status.is_empty() {
                        "not run yet"
                    } else {
                        task.last_status.as_str()
                    }
                )))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("run-task", id as u64))
                                .xsmall()
                                .label("Run now")
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.request(Request::RunTask { id })
                                })),
                        )
                        .when_some(thread, |el, thread| {
                            el.child(
                                Button::new(("task-thread", id as u64))
                                    .ghost()
                                    .xsmall()
                                    .label("Open thread")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_thread(thread, window, cx)
                                    })),
                            )
                        })
                        .child(
                            Button::new(("delete-task", id as u64))
                                .ghost()
                                .xsmall()
                                .label("Delete")
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.request(Request::DeleteTask { id })
                                })),
                        ),
                )
        });
        page("Scheduled tasks")
            .child(div().text_sm().text_color(muted).child(
                "Prompts that run on their own and post results to a thread. Nobody is there to approve, so anything that needs approval is denied.",
            ))
            .child(Textarea::new(&self.editors.task_prompt))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .child(project_button("No project".into(), None, 0))
                    .children(self.projects.iter().enumerate().map(|(ix, p)| project_button(p.name.clone(), Some(p.id), ix + 1))),
            )
            .child(h_flex().flex_wrap().gap_1().children(Provider::ALL.map(provider_button)))
            .child(Input::new(&self.editors.task_every).small())
            .child(h_flex().child(Button::new("create-task").primary().small().label("Create task").on_click(cx.listener(
                |this, _, window, cx| {
                    let prompt = this.editors.task_prompt.read(cx).value().to_string();
                    let every = this.editors.task_every.read(cx).value().trim().parse::<u32>().ok();
                    this.request(Request::CreateTask {
                        project: this.task_project,
                        provider: this.provider,
                        prompt,
                        every_minutes: every,
                    });
                    this.editors.task_prompt.update(cx, |e, cx| e.set_value("", window, cx));
                    this.editors.task_every.update(cx, |e, cx| e.set_value("", window, cx));
                },
            ))))
            .children(list)
            .into_any_element()
    }

    fn render_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        page("About")
            .child(
                div()
                    .text_sm()
                    .child(format!("Sorrel {}", env!("CARGO_PKG_VERSION"))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Data folder: {}", self.settings.data_dir.display())),
            )
            .into_any_element()
    }
}

/// A scrollable settings or project page with a heading.
fn page(title: &'static str) -> Stateful<Div> {
    v_flex()
        .id(title)
        .size_full()
        .overflow_y_scroll()
        .p_6()
        .gap_3()
        .max_w(px(760.))
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
}

fn field_label(title: &'static str, hint: &'static str, muted: Hsla) -> Div {
    v_flex()
        .pt_2()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
        .child(div().text_xs().text_color(muted).child(hint))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "now", "5m", "3h", "2d".
fn ago(seconds: i64) -> String {
    match seconds.max(0) {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.render_sidebar(cx);
        let main = match self.view {
            View::Home => self.render_home(cx),
            View::Thread(_) => match &self.thread {
                Some(view) => view.clone().into_any_element(),
                None => self.render_home(cx),
            },
            View::Project(id) => self.render_project(id, cx),
            View::Settings(section) => self.render_settings(section, cx),
        };
        let theme = cx.theme();
        let (background, foreground, border, danger, muted) = (
            theme.background,
            theme.foreground,
            theme.border,
            theme.danger,
            theme.muted_foreground,
        );
        let notices = self
            .notices
            .iter()
            .enumerate()
            .map(|(ix, (message, error))| {
                h_flex()
                    .gap_2()
                    .px_4()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .text_sm()
                    .text_color(if *error { danger } else { muted })
                    .child(div().flex_1().child(message.clone()))
                    .child(
                        Button::new(("dismiss", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.notices.remove(ix);
                                cx.notify();
                            })),
                    )
            });
        let update = self.update.clone().map(|(message, url)| {
            h_flex()
                .gap_2()
                .px_4()
                .py_1()
                .border_b_1()
                .border_color(border)
                .text_sm()
                .child(div().flex_1().child(message))
                .child(
                    Button::new("get-update")
                        .primary()
                        .xsmall()
                        .label("Download")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
        });
        h_flex()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(sidebar)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .children(update)
                    .children(notices)
                    .child(div().flex_1().min_h_0().child(main)),
            )
    }
}

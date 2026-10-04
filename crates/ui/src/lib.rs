//! The window. It renders [`Update`]s and sends [`Request`]s; it never sees a
//! vendor's wire format, spawns a process or touches the database.

mod thread;

use std::collections::VecDeque;

use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    AuthState, AuthStatus, ProjectId, ProjectInfo, Provider, Request, SettingsView, TaskInfo,
    ThreadId, ThreadInfo, Update,
};
use tokio::sync::mpsc;

pub use thread::ThreadView;

/// The screens a perf run visits.
#[derive(Clone, Copy, Debug)]
pub enum Screen {
    Home,
    Thread,
    Project,
    Tasks,
    Settings,
}

/// Notices kept on screen; older ones scroll off.
const MAX_NOTICES: usize = 4;

/// A client's link to the engine, in-process or over the daemon socket.
pub struct Connection {
    pub requests: mpsc::Sender<Request>,
    pub updates: mpsc::Receiver<Update>,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Home,
    Thread(ThreadId),
    Project(ProjectId),
    Tasks,
    Settings,
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
    view: View,
    thread: Option<Entity<ThreadView>>,
    /// Provider for new chats and tasks.
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
            Screen::Tasks => self.view = View::Tasks,
            Screen::Settings => self.view = View::Settings,
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
                self.sync_thread_info(cx);
            }
            Update::Projects(projects) => self.projects = projects,
            Update::Threads(threads) => {
                self.threads = threads;
                self.sync_thread_info(cx);
            }
            Update::Tasks(tasks) => self.tasks = tasks,
            Update::Auth(auth) => self.auth = auth,
            Update::Settings(settings) => self.settings = settings,
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

    fn sync_thread_info(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.thread {
            let id = view.read(cx).id;
            let info = self.threads.iter().find(|t| t.id == id).cloned();
            view.update(cx, |view, cx| view.set_info(info, cx));
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

    fn provider_row(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        h_flex()
            .flex_wrap()
            .gap_1()
            .children(Provider::ALL.iter().map(|&provider| {
                Button::new(("provider", provider as usize))
                    .ghost()
                    .xsmall()
                    .label(provider.label())
                    .selected(self.provider == provider)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.provider = provider;
                        cx.notify();
                    }))
            }))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let thread_button = |thread: &ThreadInfo, indent: bool| {
            let id = thread.id;
            let mark = if thread.running { "● " } else { "" };
            Button::new(("thread", id as u64))
                .ghost()
                .small()
                .w_full()
                .when(indent, |b| b.ml(px(12.)))
                .label(format!("{mark}{}", thread.title))
                .selected(self.view == View::Thread(id))
                .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)))
        };
        let section = |label: &'static str| div().pt_2().text_xs().text_color(muted).child(label);

        let mut list = v_flex()
            .id("sidebar-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_0p5()
            .child(
                h_flex().justify_between().child(section("PROJECTS")).child(
                    Button::new("add-project")
                        .ghost()
                        .xsmall()
                        .label("+")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.adding_project = !this.adding_project;
                            cx.notify();
                        })),
                ),
            );
        if self.adding_project {
            list = list.child(
                v_flex()
                    .gap_1()
                    .p_1()
                    .child(Input::new(&self.editors.new_project))
                    .child(Input::new(&self.editors.new_project_folder))
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
        for project in &self.projects {
            let id = project.id;
            list = list.child(
                Button::new(("project", id as u64))
                    .ghost()
                    .small()
                    .w_full()
                    .label(project.name.clone())
                    .selected(self.view == View::Project(id))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_project(id, window, cx)),
                    ),
            );
            for thread in self.threads.iter().filter(|t| t.project == Some(id)) {
                list = list.child(thread_button(thread, true));
            }
        }
        list = list.child(section("CHATS"));
        for thread in self.threads.iter().filter(|t| t.project.is_none()) {
            list = list.child(thread_button(thread, false));
        }

        let provider = self.provider;
        v_flex()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .gap_2()
            .p_2()
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .px_2()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Sorrel"),
            )
            .child(self.provider_row(cx))
            .child(
                Button::new("new-chat")
                    .primary()
                    .small()
                    .w_full()
                    .label("New chat")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::CreateThread {
                            project: None,
                            provider,
                        })
                    })),
            )
            .child(list)
            .child(
                Button::new("tasks")
                    .ghost()
                    .small()
                    .w_full()
                    .label("Tasks")
                    .selected(self.view == View::Tasks)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::Tasks;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .small()
                    .w_full()
                    .label("Settings")
                    .selected(self.view == View::Settings)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::Settings;
                        this.request(Request::CheckAuth);
                        cx.notify();
                    })),
            )
    }

    fn render_home(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let provider = self.provider;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("What are we working on?"))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Chats run the official CLIs you already sign in to. Projects are folders with instructions."),
            )
            .child(self.provider_row(cx))
            .child(Button::new("home-new-chat").primary().label("Start a chat").on_click(cx.listener(
                move |this, _, _, _| this.request(Request::CreateThread { project: None, provider }),
            )))
            .into_any_element()
    }

    fn render_project(&self, id: ProjectId, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(project) = self.projects.iter().find(|p| p.id == id) else {
            return div()
                .p_4()
                .child("This project was removed.")
                .into_any_element();
        };
        let provider = self.provider;
        v_flex()
            .id("project")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Project"),
            )
            .child(Input::new(&self.editors.project_name))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Folder: {}", project.folder.display())),
            )
            .child(
                div()
                    .text_sm()
                    .child("Instructions (written to CLAUDE.md and AGENTS.md in the folder)"),
            )
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
                            .label("New thread here")
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::CreateThread {
                                    project: Some(id),
                                    provider,
                                })
                            })),
                    )
                    .child(
                        Button::new("project-files")
                            .ghost()
                            .small()
                            .label("Open folder")
                            .on_click({
                                let folder = project.folder.clone();
                                move |_, _, cx| cx.open_url(&thread::file_url(&folder))
                            }),
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
            .child(self.provider_row(cx))
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
        let projects = h_flex()
            .flex_wrap()
            .gap_1()
            .child(div().text_xs().text_color(muted).child("Project:"))
            .child(project_button("None".into(), None, 0))
            .children(
                self.projects
                    .iter()
                    .enumerate()
                    .map(|(ix, p)| project_button(p.name.clone(), Some(p.id), ix + 1)),
            );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let list = v_flex().gap_2().children(self.tasks.iter().map(|task| {
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
                .p_2()
                .border_1()
                .border_color(border)
                .rounded_md()
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
        }));
        let provider = self.provider;
        v_flex()
            .id("tasks")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Tasks"))
            .child(div().text_sm().text_color(muted).child(
                "Prompts that run on their own and post results to a thread. Nobody is there to approve, so anything that needs approval is denied.",
            ))
            .child(Textarea::new(&self.editors.task_prompt))
            .child(projects)
            .child(self.provider_row(cx))
            .child(Input::new(&self.editors.task_every))
            .child(Button::new("create-task").primary().small().label("Create task").on_click(cx.listener(
                move |this, _, window, cx| {
                    let prompt = this.editors.task_prompt.read(cx).value().to_string();
                    let every = this.editors.task_every.read(cx).value().trim().parse::<u32>().ok();
                    this.request(Request::CreateTask { project: this.task_project, provider, prompt, every_minutes: every });
                    this.editors.task_prompt.update(cx, |e, cx| e.set_value("", window, cx));
                    this.editors.task_every.update(cx, |e, cx| e.set_value("", window, cx));
                },
            )))
            .child(list)
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted, danger) = (theme.border, theme.muted_foreground, theme.danger);
        let accounts = v_flex()
            .gap_2()
            .children(Provider::ALL.iter().enumerate().map(|(ix, &provider)| {
                let status = self.auth.iter().find(|a| a.provider == provider);
                let (state, detail) = status
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
                    .gap_1()
                    .p_2()
                    .border_1()
                    .border_color(border)
                    .rounded_md()
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
                                    .text_sm()
                                    .text_color(
                                        if matches!(
                                            state,
                                            AuthState::Missing | AuthState::SignedOut
                                        ) {
                                            danger
                                        } else {
                                            muted
                                        },
                                    )
                                    .child(state_label),
                            )
                            .child(div().flex_1().text_xs().text_color(muted).child(detail))
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
                            .children(
                                key.clone()
                                    .map(|key| div().flex_1().child(Input::new(&key))),
                            )
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
            }));
        let heading =
            |text: &'static str| div().pt_2().font_weight(FontWeight::SEMIBOLD).child(text);
        v_flex()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Settings"))
            .child(heading("Accounts"))
            .child(div().text_xs().text_color(muted).child(
                "Sign-in happens inside each official CLI; Sorrel never sees those tokens. API keys you enter here are stored only on this machine.",
            ))
            .child(Button::new("check-auth").xsmall().label("Check again").on_click(cx.listener(
                |this, _, _, _| this.request(Request::CheckAuth),
            )))
            .child(accounts)
            .child(heading("Memory"))
            .child(div().text_xs().text_color(muted).child("Shared with every session through CLAUDE.md and AGENTS.md."))
            .child(Textarea::new(&self.editors.memory))
            .child(Button::new("save-memory").small().label("Save memory").on_click(cx.listener(
                |this, _, _, cx| {
                    let text = this.editors.memory.read(cx).value().to_string();
                    this.request(Request::SetMemory { text });
                },
            )))
            .child(heading("Connectors (MCP servers)"))
            .child(div().text_xs().text_color(muted).child("A JSON list of servers, passed to every CLI when a session starts."))
            .child(Textarea::new(&self.editors.mcp))
            .child(Button::new("save-mcp").small().label("Save connectors").on_click(cx.listener(
                |this, _, _, cx| {
                    let json = this.editors.mcp.read(cx).value().to_string();
                    this.request(Request::SetMcpServers { json });
                },
            )))
            .child(heading("Data"))
            .child(div().text_xs().text_color(muted).child(format!(
                "{} · up to {} sessions at once · version {}",
                self.settings.data_dir.display(),
                self.settings.max_sessions,
                env!("CARGO_PKG_VERSION")
            )))
            .into_any_element()
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
            View::Tasks => self.render_tasks(cx),
            View::Settings => self.render_settings(cx),
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
                            .label("Dismiss")
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

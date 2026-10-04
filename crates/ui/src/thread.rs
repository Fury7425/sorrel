//! One conversation: a centered timeline and the composer under it.
//!
//! Code threads fold tool activity into one "worked" row per stretch and put
//! every control in the composer — model, effort (up to Ultrathink and
//! Ultracode), fast mode, context window, Build/Plan, access, steer, queue
//! and stop — plus the approval or question waiting on the user. Chat threads
//! show the same log as a plain conversation: messages, web lookups as one
//! line, and files the assistant made as cards.
//!
//! A draft (id 0) is the new-thread screen: its first message creates the
//! thread.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    message_scroller::{MessageScroller, MessageScrollerState},
    popover::Popover,
    switch::Switch,
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    Access, AgentEvent, BlobRef, Delivery, FileContent, FileEntry, Item, Mode, ModelInfo,
    PermChoice, ProjectId, Provider, Request, Seq, StopReason, ThreadEvent, ThreadId, ThreadInfo,
    TodoStatus, ToolKind, ToolStatus, Transcript, TurnSettings, ULTRACODE, ULTRATHINK,
};
use tokio::sync::mpsc;

use crate::style::{self, provider_icon, provider_tile, segment, segmented, ultra_hue};

/// Width of the timeline and composer column.
const COLUMN: f32 = 780.;
const CHAT_COLUMN: f32 = 720.;
/// Full tool outputs kept after "show all"; older ones are dropped.
const MAX_EXPANDED: usize = 8;
/// Slash commands shown at once.
const MAX_COMMANDS: usize = 8;

/// A display row: one item, or a stretch of tool calls and thinking folded
/// into one "worked" row (`start..end` are item indices).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Item(usize),
    Work { start: usize, end: usize },
}

fn is_work(item: &Item) -> bool {
    matches!(item, Item::Tool { .. } | Item::Thinking { .. })
}

/// Rows for `items[from..]`.
fn build_rows(items: &[Item], from: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut ix = from;
    while ix < items.len() {
        if is_work(&items[ix]) {
            let start = ix;
            while ix < items.len() && is_work(&items[ix]) {
                ix += 1;
            }
            rows.push(Row::Work { start, end: ix });
        } else {
            rows.push(Row::Item(ix));
            ix += 1;
        }
    }
    rows
}

fn row_start(row: Row) -> usize {
    match row {
        Row::Item(ix) => ix,
        Row::Work { start, .. } => start,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Picker {
    Model,
    Access,
    Project,
}

enum Pane {
    None,
    Files,
    Diff(u32, SharedString),
}

/// What the workspace tells a thread about its surroundings.
#[derive(Clone, Default, PartialEq)]
pub struct Look {
    /// A wallpaper is showing, so the new-thread screen drops its heading.
    pub wallpaper: bool,
    /// Projects a draft can start in.
    pub projects: Vec<(ProjectId, String)>,
    /// Enter steers a running turn; Ctrl+Enter queues. Off: the other way round.
    pub enter_steers: bool,
    /// CLIs switched on in Settings.
    pub enabled: Vec<Provider>,
}

pub struct ThreadView {
    pub id: ThreadId,
    requests: mpsc::Sender<Request>,
    info: Option<ThreadInfo>,
    transcript: Transcript,
    rows: Vec<Row>,
    /// Text for each item, built once and shared with the renderer, so
    /// unchanged rows never re-parse.
    cache: Vec<Option<SharedString>>,
    /// The newest assistant row owns markdown state it appends to while it
    /// streams. Every other row uses keyed state that GPUI drops off screen.
    live: Option<(usize, Entity<TextViewState>)>,
    /// Worked rows the user opened, by their first item.
    opened: HashSet<usize>,
    scroller: Entity<MessageScrollerState>,
    older: Option<Seq>,
    composer: Entity<TextareaState>,
    settings: TurnSettings,
    settings_loaded: bool,
    catalog: HashMap<Provider, Vec<ModelInfo>>,
    favorites: Vec<(Provider, String)>,
    commands: HashMap<Provider, Vec<String>>,
    look: Look,
    picker: Option<Picker>,
    /// The model popover shows the model list instead of effort and options.
    model_list: bool,
    model_search: Entity<InputState>,
    /// The model list's provider tab; `None` shows favorites.
    rail: Option<Provider>,
    expanded: Vec<(String, SharedString)>,
    drafts: HashMap<String, Vec<Vec<String>>>,
    question_page: HashMap<String, usize>,
    pane: Pane,
    files: Vec<FileEntry>,
    file: Option<(String, FileContent, SharedString)>,
    turns_ended: usize,
    _subscriptions: Vec<Subscription>,
}

impl ThreadView {
    pub fn new(
        id: ThreadId,
        info: Option<ThreadInfo>,
        requests: mpsc::Sender<Request>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 10)
                .submit_on_enter(true)
                .placeholder("Ask for changes, steer, or queue a follow-up")
        });
        let model_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search models"));
        // Not focused on open: a focused composer blinks its caret, which redraws an idle window.
        let _subscriptions = vec![
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter {
                        shift: false,
                        secondary,
                    } => {
                        let delivery = if this.look.enter_steers != *secondary {
                            Delivery::SteerNow
                        } else {
                            Delivery::Queue
                        };
                        this.send(delivery, window, cx);
                    }
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe(&model_search, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            }),
        ];
        let mut view = Self {
            id,
            requests,
            info: None,
            transcript: Transcript::default(),
            rows: Vec::new(),
            cache: Vec::new(),
            live: None,
            opened: HashSet::new(),
            scroller,
            older: None,
            composer,
            settings: TurnSettings::default(),
            settings_loaded: false,
            catalog: HashMap::new(),
            favorites: Vec::new(),
            commands: HashMap::new(),
            look: Look::default(),
            picker: None,
            model_list: false,
            model_search,
            rail: None,
            expanded: Vec::new(),
            drafts: HashMap::new(),
            question_page: HashMap::new(),
            pane: Pane::None,
            files: Vec::new(),
            file: None,
            turns_ended: 0,
            _subscriptions,
        };
        view.set_info(info, cx);
        view
    }

    /// The new-thread screen for Code (`chat == false`) or Chat.
    pub fn draft(
        chat: bool,
        provider: Provider,
        settings: TurnSettings,
        requests: mpsc::Sender<Request>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let info = ThreadInfo {
            id: 0,
            project: None,
            title: String::new(),
            provider,
            folder: Default::default(),
            running: false,
            needs_input: false,
            queued: 0,
            updated_at: 0,
            pinned: false,
            archived: false,
            failed: false,
            settings,
            chat,
        };
        let view = Self::new(0, Some(info), requests, window, cx);
        let placeholder = if chat {
            "How can I help?"
        } else {
            "Do anything…"
        };
        view.composer.update(cx, |composer, cx| {
            composer.set_placeholder(placeholder, window, cx)
        });
        view
    }

    /// Gives an untouched draft the CLI and choices new threads start with.
    pub fn reset_draft(
        &mut self,
        provider: Provider,
        settings: TurnSettings,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_draft() {
            return;
        }
        if let Some(info) = self.info.as_mut() {
            info.provider = provider;
        }
        self.settings = settings;
        self.rail = Some(provider);
        cx.notify();
    }

    fn is_draft(&self) -> bool {
        self.id == 0
    }

    pub fn is_chat(&self) -> bool {
        self.info.as_ref().is_some_and(|info| info.chat)
    }

    fn request(&self, request: Request) {
        let _ = self.requests.try_send(request);
    }

    fn provider(&self) -> Provider {
        self.info
            .as_ref()
            .map_or(Provider::Claude, |info| info.provider)
    }

    fn running(&self) -> bool {
        self.info.as_ref().is_some_and(|info| info.running)
    }

    pub fn set_info(&mut self, info: Option<ThreadInfo>, cx: &mut Context<Self>) {
        if let Some(info) = &info
            && !self.settings_loaded
        {
            self.settings = info.settings.clone();
            self.settings_loaded = true;
            self.rail = Some(info.provider);
        }
        if self.info != info {
            self.info = info;
            cx.notify();
        }
    }

    /// Points a draft at a project (or none).
    pub fn set_project(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        if let Some(info) = self.info.as_mut().filter(|_| self.id == 0) {
            info.project = project;
            cx.notify();
        }
    }

    pub fn project(&self) -> Option<ProjectId> {
        self.info.as_ref().and_then(|info| info.project)
    }

    pub fn set_catalog(
        &mut self,
        catalog: HashMap<Provider, Vec<ModelInfo>>,
        favorites: Vec<(Provider, String)>,
        commands: HashMap<Provider, Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        self.catalog = catalog;
        self.favorites = favorites;
        self.commands = commands;
        cx.notify();
    }

    pub fn set_look(&mut self, look: Look, cx: &mut Context<Self>) {
        if self.look != look {
            self.look = look;
            cx.notify();
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn turns_ended(&self) -> usize {
        self.turns_ended
    }

    pub fn scroll_to_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.scroller.update(cx, |scroller, cx| {
            scroller.scroll_to_item(ix, cx);
        });
    }

    /// Opens or closes the side panel; it opens on the thread's files.
    pub fn toggle_pane(&mut self, cx: &mut Context<Self>) {
        if matches!(self.pane, Pane::None) {
            self.request(Request::ListFiles { thread: self.id });
        } else {
            self.pane = Pane::None;
        }
        cx.notify();
    }

    pub fn pane_open(&self) -> bool {
        !matches!(self.pane, Pane::None)
    }

    fn set_settings(&mut self, settings: TurnSettings, cx: &mut Context<Self>) {
        self.settings = settings;
        if !self.is_draft() {
            self.request(Request::SetThreadSettings {
                thread: self.id,
                settings: self.settings.clone(),
            });
        }
        cx.notify();
    }

    /// Moves a thread with no messages (or the draft) to another CLI.
    fn set_provider(&mut self, provider: Provider, cx: &mut Context<Self>) {
        if provider == self.provider() || !self.transcript.items.is_empty() {
            return;
        }
        if let Some(info) = self.info.as_mut() {
            info.provider = provider;
        }
        if !self.is_draft() {
            self.request(Request::SetThreadProvider {
                thread: self.id,
                provider,
            });
        }
        if !self.catalog.contains_key(&provider) {
            self.request(Request::ListModels { provider });
        }
        let settings = TurnSettings {
            model: None,
            effort: None,
            fast: false,
            long_context: false,
            ..self.settings.clone()
        };
        self.set_settings(settings, cx);
    }

    pub fn apply_page(
        &mut self,
        events: Vec<ThreadEvent>,
        turn: u32,
        prepend: bool,
        older: Option<Seq>,
        cx: &mut Context<Self>,
    ) {
        let mut page = Transcript::starting_at(turn);
        for event in &events {
            page.apply(event);
        }
        self.older = older;
        let n = page.items.len();
        if prepend {
            let before = self.rows.len();
            self.transcript.prepend(page.items);
            self.cache.splice(0..0, std::iter::repeat_n(None, n));
            if let Some((ix, _)) = &mut self.live {
                *ix += n;
            }
            self.opened = self.opened.iter().map(|start| start + n).collect();
            self.rows = build_rows(&self.transcript.items, 0);
            let added = self.rows.len().saturating_sub(before);
            self.scroller.update(cx, |scroller, cx| {
                scroller.prepend(added, cx);
            });
        } else {
            self.transcript = page;
            self.cache = vec![None; n];
            self.live = None;
            self.opened.clear();
            self.rows = build_rows(&self.transcript.items, 0);
            let rows = self.rows.len();
            self.scroller
                .update(cx, |scroller, cx| scroller.reset(rows, cx));
        }
        cx.notify();
    }

    pub fn apply_event(&mut self, event: ThreadEvent, cx: &mut Context<Self>) {
        if matches!(event, ThreadEvent::Agent(AgentEvent::TurnEnded { .. })) {
            self.turns_ended += 1;
        }
        let Some(ix) = self.transcript.apply(&event) else {
            return;
        };
        let len = self.transcript.items.len();
        if len > self.cache.len() {
            self.cache.resize(len, None);
        } else {
            self.cache[ix] = None;
        }
        self.refresh_rows(ix, cx);

        if let (
            ThreadEvent::Agent(AgentEvent::TextDelta { text, .. }),
            Item::Assistant { text: full, .. },
        ) = (&event, &self.transcript.items[ix])
        {
            let streaming = self
                .live
                .as_ref()
                .filter(|(live_ix, _)| *live_ix == ix)
                .map(|(_, state)| state.clone());
            match streaming {
                Some(state) => state.update(cx, |state, cx| state.push_str(text, cx)),
                None => {
                    let state = cx.new(|cx| TextViewState::markdown(full, cx));
                    self.live = Some((ix, state));
                }
            }
        }
        cx.notify();
    }

    /// Rebuilds the rows from the one holding item `changed` to the end.
    /// Streaming only ever changes the tail, so this stays cheap.
    fn refresh_rows(&mut self, changed: usize, cx: &mut Context<Self>) {
        let first = self
            .rows
            .iter()
            .rposition(|row| row_start(*row) <= changed)
            .unwrap_or(0);
        let from = self.rows.get(first).map_or(0, |row| row_start(*row));
        let tail = build_rows(&self.transcript.items, from);
        let old = first..self.rows.len();
        if self.rows[old.clone()] == tail[..] {
            let row = first
                + tail
                    .iter()
                    .position(|r| row_start(*r) >= changed)
                    .unwrap_or(0);
            let row = row.min(self.rows.len().saturating_sub(1));
            self.scroller.update(cx, |scroller, cx| {
                scroller.remeasure_items(row..row + 1, cx);
            });
        } else {
            let count = tail.len();
            self.rows.splice(old.clone(), tail);
            self.scroller.update(cx, |scroller, cx| {
                scroller.splice(old, count, cx);
            });
        }
    }

    pub fn show_blob(&mut self, blob: &BlobRef, text: String, cx: &mut Context<Self>) {
        let call_id = self.transcript.items.iter().find_map(|item| match item {
            Item::Tool {
                call_id,
                output: Some(output),
                ..
            } if output == blob => Some(call_id.clone()),
            _ => None,
        });
        if let Some(call_id) = call_id {
            if self.expanded.len() == MAX_EXPANDED {
                self.expanded.remove(0);
            }
            // ponytail: huge outputs are cut for display; the blob keeps all of it.
            let text: String = text.chars().take(200_000).collect();
            self.expanded.push((call_id, text.into()));
            self.cache.iter_mut().for_each(|slot| *slot = None);
            self.scroller
                .update(cx, |scroller, cx| scroller.remeasure(cx));
            cx.notify();
        }
    }

    pub fn show_files(&mut self, files: Vec<FileEntry>, cx: &mut Context<Self>) {
        self.files = files;
        self.pane = Pane::Files;
        cx.notify();
    }

    pub fn show_file(&mut self, path: String, content: FileContent, cx: &mut Context<Self>) {
        let text: SharedString = match &content {
            FileContent::Markdown(text) => text.clone().into(),
            FileContent::Text { text, ext } => format!("```{ext}\n{text}\n```").into(),
            _ => SharedString::default(),
        };
        self.file = Some((path, content, text));
        self.pane = Pane::Files;
        cx.notify();
    }

    pub fn show_diff(&mut self, turn: u32, text: String, cx: &mut Context<Self>) {
        let text: String = text.chars().take(400_000).collect();
        self.pane = Pane::Diff(turn, format!("```diff\n{text}\n```").into());
        cx.notify();
    }

    fn send(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        if self.is_draft() {
            let Some(info) = self.info.clone() else {
                return;
            };
            self.request(Request::StartThread {
                project: info.project,
                provider: info.provider,
                text,
                settings: self.settings.clone(),
                chat: info.chat,
            });
            return;
        }
        self.request(Request::Send {
            thread: self.id,
            text,
            settings: self.settings.clone(),
            delivery,
        });
        self.scroller
            .update(cx, |scroller, cx| scroller.scroll_to_end(cx));
    }

    /// The item's text as a shared string, built on first use.
    fn text(&mut self, ix: usize) -> SharedString {
        if let Some(Some(text)) = self.cache.get(ix) {
            return text.clone();
        }
        let text: SharedString = match &self.transcript.items[ix] {
            Item::User { text, .. }
            | Item::Assistant { text, .. }
            | Item::Thinking { text, .. } => text.clone().into(),
            Item::Tool {
                call_id, preview, ..
            } => self
                .expanded
                .iter()
                .find(|(id, _)| id == call_id)
                .map(|(_, text)| text.clone())
                .unwrap_or_else(|| preview.clone().into()),
            Item::Permission { detail, .. } => detail.clone().into(),
            _ => SharedString::default(),
        };
        if let Some(slot) = self.cache.get_mut(ix) {
            *slot = Some(text.clone());
        }
        text
    }

    /// The approval or question the agent is waiting on, if any.
    fn pending(&self) -> Option<usize> {
        if !self.info.as_ref().is_some_and(|info| info.needs_input) {
            return None;
        }
        self.transcript.items.iter().rposition(|item| {
            matches!(
                item,
                Item::Permission { resolved: None, .. } | Item::Question { answers: None, .. }
            )
        })
    }

    fn column(&self) -> f32 {
        if self.is_chat() { CHAT_COLUMN } else { COLUMN }
    }

    fn render_row(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(row_ix).copied() else {
            return div().into_any_element();
        };
        let chat = self.is_chat();
        let body = match (row, chat) {
            (Row::Item(ix), _) => self.render_item(ix, cx),
            (Row::Work { start, end }, false) => self.render_work(start, end, cx),
            (Row::Work { start, end }, true) => self.render_chat_work(start, end, cx),
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px_6()
            .py_2()
            .child(div().w_full().max_w(px(self.column())).child(body))
            .into_any_element()
    }

    fn render_item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.transcript.items.get(ix).cloned() else {
            return div().into_any_element();
        };
        let text = self.text(ix);
        let theme = cx.theme();
        let (muted, danger, fg) = (theme.muted_foreground, theme.danger, theme.foreground);
        let thread = self.id;
        let running = self.running();
        let chat = self.is_chat();
        let provider = self.provider();
        match item {
            Item::User { .. } => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.78))
                        .px_4()
                        .py_2p5()
                        .rounded(px(18.))
                        .rounded_br(px(6.))
                        .bg(fg.opacity(0.08))
                        .border_1()
                        .border_color(fg.opacity(0.09))
                        .child(text),
                )
                .into_any_element(),
            Item::Assistant { .. } => {
                let body = match &self.live {
                    Some((live_ix, state)) if *live_ix == ix => {
                        TextView::new(state).into_any_element()
                    }
                    _ => TextView::markdown(("md", ix), text).into_any_element(),
                };
                if chat {
                    h_flex()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .mt_0p5()
                                .child(provider_tile(provider, 26.).rounded_full()),
                        )
                        .child(div().flex_1().min_w_0().child(body))
                        .into_any_element()
                } else {
                    body
                }
            }
            Item::Permission {
                title,
                plan: true,
                resolved,
                ..
            } => style::glass(cx)
                .p_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().text_xs().text_color(muted).child("PLAN"))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(div().flex_1())
                        .when_some(resolved, |el, choice| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(resolved_label(choice, true)),
                            )
                        }),
                )
                .child(TextView::markdown(("plan", ix), text))
                .into_any_element(),
            Item::Permission {
                title, resolved, ..
            } => h_flex()
                .gap_2()
                .text_xs()
                .text_color(muted)
                .child(match resolved {
                    Some(choice) => format!("{} · {title}", resolved_label(choice, false)),
                    None => format!("Waiting for approval · {title}"),
                })
                .into_any_element(),
            Item::Question {
                questions, answers, ..
            } => {
                let summary = match &answers {
                    Some(answers) => questions
                        .iter()
                        .zip(answers)
                        .map(|(q, a)| format!("{}: {}", q.header, a.join(", ")))
                        .collect::<Vec<_>>()
                        .join(" · "),
                    None => "Waiting for your answer".into(),
                };
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(summary)
                    .into_any_element()
            }
            Item::Todo { items } => style::glass(cx)
                .p_3()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(muted).child("TASKS"))
                .children(items.into_iter().map(|todo| {
                    let mark = match todo.status {
                        TodoStatus::Pending => "○",
                        TodoStatus::InProgress => "◐",
                        TodoStatus::Done => "●",
                    };
                    div()
                        .text_sm()
                        .when(todo.status == TodoStatus::Done, |el| el.text_color(muted))
                        .child(format!("{mark}  {}", todo.text))
                }))
                .into_any_element(),
            Item::Error { message } => div()
                .px_3()
                .py_2()
                .rounded_lg()
                .border_1()
                .border_color(danger.opacity(0.5))
                .bg(danger.opacity(0.08))
                .text_sm()
                .text_color(danger)
                .child(message)
                .into_any_element(),
            Item::TurnEnd { .. } if chat => div().into_any_element(),
            Item::TurnEnd {
                turn,
                reason,
                input,
                output,
            } => h_flex()
                .gap_2()
                .text_xs()
                .text_color(muted)
                .child(format!(
                    "{} · {} in / {} out",
                    reason_label(reason),
                    tokens(input),
                    tokens(output)
                ))
                .when(!running, |el| {
                    el.child(
                        Button::new(("diff", ix))
                            .ghost()
                            .xsmall()
                            .label("Review changes")
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::Diff { thread, turn })
                            })),
                    )
                    .child(
                        Button::new(("undo", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Undo)
                            .label("Undo")
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::Restore { thread, turn })
                            })),
                    )
                })
                .into_any_element(),
            Item::Restored { turn } => div()
                .text_xs()
                .text_color(muted)
                .child(format!(
                    "Files restored to how they were before turn {turn}."
                ))
                .into_any_element(),
            // Tool calls and thinking render inside their worked row.
            Item::Tool { .. } | Item::Thinking { .. } => div().into_any_element(),
        }
    }

    fn render_work(&mut self, start: usize, end: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, danger, mono, fg, primary) = (
            theme.muted_foreground,
            theme.danger,
            theme.mono_font_family.clone(),
            theme.foreground,
            theme.primary,
        );
        let open = self.opened.contains(&start);
        let active = self.running() && end == self.transcript.items.len();
        let steps = (start..end)
            .filter(|&ix| matches!(self.transcript.items[ix], Item::Tool { .. }))
            .count();
        let label = format!(
            "{} · {steps} step{}",
            if active { "Working" } else { "Worked" },
            if steps == 1 { "" } else { "s" }
        );
        let header = h_flex()
            .id(("work", start))
            .gap_2()
            .h(px(28.))
            .px_2p5()
            .rounded_full()
            .bg(fg.opacity(0.04))
            .border_1()
            .border_color(fg.opacity(0.06))
            .text_sm()
            .text_color(if active { primary } else { muted })
            .cursor_pointer()
            .when(active, |el| {
                el.child(style::thinking_orb(("orb", start), 14., primary))
            })
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall(),
            )
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.opened.remove(&start) {
                    this.opened.insert(start);
                }
                if let Some(row) = this.rows.iter().position(|r| row_start(*r) == start) {
                    this.scroller.update(cx, |scroller, cx| {
                        scroller.remeasure_items(row..row + 1, cx);
                    });
                }
                cx.notify();
            }));
        let header = h_flex().child(header);
        if !open {
            return header.into_any_element();
        }
        let mut steps_view = v_flex().gap_2p5();
        for ix in start..end {
            let text = self.text(ix);
            let step = match self.transcript.items[ix].clone() {
                Item::Thinking { .. } => {
                    let short: String = text.chars().take(600).collect();
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(short)
                        .into_any_element()
                }
                Item::Tool {
                    kind,
                    title,
                    status,
                    output,
                    call_id,
                    ..
                } => {
                    let expanded = self.expanded.iter().any(|(id, _)| *id == call_id);
                    let tag = match kind {
                        ToolKind::Edit => primary,
                        _ => fg,
                    };
                    v_flex()
                        .gap_1p5()
                        .child(
                            h_flex()
                                .gap_2p5()
                                .child(
                                    div()
                                        .w(px(48.))
                                        .flex_shrink_0()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(tag.opacity(0.12))
                                        .text_xs()
                                        .text_center()
                                        .text_color(tag)
                                        .child(kind_label(kind)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .font_family(mono.clone())
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if status == ToolStatus::Failed {
                                            danger
                                        } else {
                                            muted
                                        })
                                        .child(status_label(status)),
                                ),
                        )
                        .when(!text.is_empty(), |el| {
                            el.child(
                                div()
                                    .ml(px(58.))
                                    .px_3()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(fg.opacity(0.04))
                                    .border_1()
                                    .border_color(fg.opacity(0.06))
                                    .text_xs()
                                    .font_family(mono.clone())
                                    .text_color(muted)
                                    .child(text),
                            )
                        })
                        .when_some(output.filter(|_| !expanded), |el, blob| {
                            el.child(
                                h_flex().ml(px(58.)).child(
                                    Button::new(("show-all", ix))
                                        .ghost()
                                        .xsmall()
                                        .label("Show all output")
                                        .on_click(cx.listener(move |this, _, _, _| {
                                            this.request(Request::ReadBlob { blob: blob.clone() })
                                        })),
                                ),
                            )
                        })
                        .into_any_element()
                }
                _ => div().into_any_element(),
            };
            steps_view = steps_view.child(step);
        }
        v_flex()
            .gap_3()
            .child(header)
            .child(style::glass(cx).p_3().child(steps_view))
            .into_any_element()
    }

    /// A chat's tool stretch: lookups as one quiet line, files as cards.
    fn render_chat_work(&mut self, start: usize, end: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, primary, fg) = (theme.muted_foreground, theme.primary, theme.foreground);
        let active = self.running() && end == self.transcript.items.len();
        let folder = self
            .info
            .as_ref()
            .map(|info| info.folder.clone())
            .unwrap_or_default();
        let mut lookups = 0;
        let mut files: Vec<String> = Vec::new();
        for item in &self.transcript.items[start..end] {
            if let Item::Tool { kind, title, .. } = item {
                match kind {
                    ToolKind::Edit => {
                        let path = title.split_once(": ").map_or(title.as_str(), |(_, p)| p);
                        if !files.iter().any(|f| f == path) {
                            files.push(path.to_owned());
                        }
                    }
                    ToolKind::Search | ToolKind::Fetch | ToolKind::Read => lookups += 1,
                    _ => {}
                }
            }
        }
        let status = if active {
            Some("Thinking…".to_owned())
        } else if lookups > 0 {
            Some(format!(
                "Looked at {lookups} source{}",
                if lookups == 1 { "" } else { "s" }
            ))
        } else {
            None
        };
        v_flex()
            .gap_2()
            .pl(px(38.))
            .when_some(status, |el, status| {
                el.child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .text_color(if active { primary } else { muted })
                        .when(active, |el| {
                            el.child(style::thinking_orb(("chat-orb", start), 14., primary))
                        })
                        .child(status),
                )
            })
            .children(files.into_iter().enumerate().map(|(i, path)| {
                let full = {
                    let p = std::path::Path::new(&path);
                    if p.is_absolute() {
                        p.to_owned()
                    } else {
                        folder.join(p)
                    }
                };
                let name = full
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(path.clone());
                let url = file_url(&full);
                h_flex()
                    .gap_3()
                    .p_3()
                    .rounded_xl()
                    .bg(theme.background.opacity(0.55))
                    .border_1()
                    .border_color(fg.opacity(0.08))
                    .child(
                        div()
                            .size(px(36.))
                            .rounded_lg()
                            .bg(primary.opacity(0.16))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(Lucide::FileText).small().text_color(primary)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(name),
                            )
                            .child(div().text_xs().text_color(muted).truncate().child(path)),
                    )
                    .child(
                        Button::new(("open-artifact", start * 100 + i))
                            .small()
                            .label("Open")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
            }))
            .into_any_element()
    }

    /// The approval or question panel shown inside the composer.
    fn render_pending(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg, warning, primary) = (
            theme.muted_foreground,
            theme.foreground,
            theme.warning,
            theme.primary,
        );
        let thread = self.id;
        let detail = self.text(ix);
        let panel = v_flex()
            .gap_2()
            .p_3()
            .rounded_xl()
            .bg(fg.opacity(0.05))
            .border_1()
            .border_color(fg.opacity(0.1));
        match self.transcript.items[ix].clone() {
            Item::Permission {
                req_id,
                title,
                plan,
                ..
            } => {
                let kind = if plan {
                    "PLAN READY"
                } else if title.starts_with("Run")
                    || title.starts_with("Bash")
                    || title.starts_with("PowerShell")
                {
                    "COMMAND APPROVAL"
                } else if title.starts_with("Edit")
                    || title.starts_with("Write")
                    || title.contains("file")
                {
                    "FILE CHANGE APPROVAL"
                } else {
                    "PERMISSION REQUEST"
                };
                let choice = |id: &'static str, label: &'static str, choice: PermChoice| {
                    let req_id = req_id.clone();
                    Button::new(id).small().label(label).on_click(cx.listener(
                        move |this, _, _, _| {
                            this.request(Request::Resolve {
                                thread,
                                req_id: req_id.clone(),
                                choice,
                            })
                        },
                    ))
                };
                let actions = if plan {
                    h_flex()
                        .gap_2()
                        .child(choice("implement", "Implement", PermChoice::AllowOnce).primary())
                        .child(choice("refine", "Refine", PermChoice::Deny).ghost())
                } else {
                    h_flex()
                        .gap_2()
                        .child(choice("approve", "Approve", PermChoice::AllowOnce).primary())
                        .child(
                            choice(
                                "always",
                                "Always allow this session",
                                PermChoice::AllowAlways,
                            )
                            .outline(),
                        )
                        .child(choice("decline", "Decline", PermChoice::Deny).ghost())
                };
                panel
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(warning)
                            .child(kind),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when(!plan && !detail.is_empty(), |el| {
                        el.child(
                            div()
                                .id("pending-detail")
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .child(TextView::markdown(("pending", ix), detail)),
                        )
                    })
                    .child(actions)
                    .into_any_element()
            }
            Item::Question {
                req_id, questions, ..
            } => {
                let page = self
                    .question_page
                    .get(&req_id)
                    .copied()
                    .unwrap_or(0)
                    .min(questions.len().saturating_sub(1));
                let draft = self
                    .drafts
                    .get(&req_id)
                    .cloned()
                    .unwrap_or_else(|| vec![Vec::new(); questions.len()]);
                let Some(question) = questions.get(page).cloned() else {
                    return div().into_any_element();
                };
                let count = questions.len();
                let mut options = h_flex().gap_1().flex_wrap();
                for (oi, option) in question.options.iter().enumerate() {
                    let picked = draft.get(page).is_some_and(|a| a.contains(option));
                    let (req_id, option, multi) = (req_id.clone(), option.clone(), question.multi);
                    options = options.child(
                        Button::new(("option", page * 100 + oi))
                            .small()
                            .outline()
                            .label(option.clone())
                            .selected(picked)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let draft = this
                                    .drafts
                                    .entry(req_id.clone())
                                    .or_insert_with(|| vec![Vec::new(); count]);
                                let slot = &mut draft[page];
                                if multi {
                                    match slot.iter().position(|o| *o == option) {
                                        Some(at) => {
                                            slot.remove(at);
                                        }
                                        None => slot.push(option.clone()),
                                    }
                                } else {
                                    *slot = vec![option.clone()];
                                }
                                cx.notify();
                            })),
                    );
                }
                let last = page + 1 >= count;
                let advance = {
                    let req_id = req_id.clone();
                    Button::new("question-next")
                        .primary()
                        .small()
                        .label(if last {
                            if count > 1 {
                                "Submit answers"
                            } else {
                                "Submit answer"
                            }
                        } else {
                            "Next question"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if last {
                                let answers = this.drafts.remove(&req_id).unwrap_or_default();
                                this.question_page.remove(&req_id);
                                this.request(Request::Answer {
                                    thread,
                                    req_id: req_id.clone(),
                                    answers,
                                });
                            } else {
                                this.question_page.insert(req_id.clone(), page + 1);
                            }
                            cx.notify();
                        }))
                };
                panel
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(
                                div()
                                    .text_color(primary)
                                    .child(question.header.to_uppercase()),
                            )
                            .when(count > 1, |el| {
                                el.child(
                                    div()
                                        .text_color(muted)
                                        .child(format!("{} of {count}", page + 1)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(question.text.clone()),
                    )
                    .when(question.multi, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Select one or more options."),
                        )
                    })
                    .child(options)
                    .child(h_flex().gap_2().child(advance))
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    /// The selected model's entry, or the provider's first when on default.
    fn model_info(&self) -> Option<ModelInfo> {
        let models = self.catalog.get(&self.provider())?;
        match &self.settings.model {
            Some(id) => models.iter().find(|m| m.id == *id).cloned(),
            None => models.first().cloned(),
        }
    }

    fn model_label(&self) -> String {
        match &self.settings.model {
            None => "Default".to_owned(),
            Some(id) => self
                .catalog
                .get(&self.provider())
                .and_then(|models| models.iter().find(|m| m.id == *id))
                .map_or(id.clone(), |m| m.label.clone()),
        }
    }

    fn render_model_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let open = self.picker == Some(Picker::Model);
        let content = if self.model_list {
            self.render_model_list(cx)
        } else {
            self.render_model_options(cx)
        };
        let theme = cx.theme();
        let (muted, fg, popover, warning) = (
            theme.muted_foreground,
            theme.foreground,
            theme.popover,
            theme.warning,
        );
        let provider = self.provider();
        let effort = self.settings.effort.clone();
        let think = effort.as_deref() == Some(ULTRATHINK);
        let effort_text = effort.as_deref().map(effort_label).unwrap_or("").to_owned();
        let trigger = Button::new("model-trigger").ghost().small().child(
            h_flex()
                .gap_1p5()
                .child(provider_icon(provider, 14.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(self.model_label()),
                )
                .when(!effort_text.is_empty(), |el| {
                    el.child(
                        div()
                            .text_color(if think { ultra_hue(0.25) } else { muted })
                            .child(effort_text),
                    )
                })
                .when(self.settings.fast, |el| {
                    el.child(Icon::new(Lucide::Zap).xsmall().text_color(warning))
                }),
        );
        Popover::new("model-picker")
            .anchor(Anchor::BottomRight)
            .open(open)
            .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                this.picker = open.then_some(Picker::Model);
                this.model_list = false;
                let provider = this.provider();
                if *open && !this.catalog.contains_key(&provider) {
                    this.request(Request::ListModels { provider });
                }
                cx.notify();
            }))
            .trigger(trigger)
            .w(px(if self.model_list { 460. } else { 320. }))
            .p_3()
            .bg(popover.opacity(0.94))
            .border_color(fg.opacity(0.1))
            .child(content)
            .into_any_element()
    }

    /// Effort slider, fast mode and context window.
    fn render_model_options(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg, primary, warning) = (
            theme.muted_foreground,
            theme.foreground,
            theme.primary,
            theme.warning,
        );
        let provider = self.provider();
        let claude = provider == Provider::Claude;
        let efforts = self.model_info().map(|m| m.efforts).unwrap_or_default();
        let current = self.settings.effort.clone();
        let selected = current
            .as_ref()
            .and_then(|e| efforts.iter().position(|x| x == e));
        let think = current.as_deref() == Some(ULTRATHINK);
        let code = current.as_deref() == Some(ULTRACODE);
        let pink: Hsla = rgb(0xec6fae).into();

        let title = div().text_base().font_weight(FontWeight::SEMIBOLD).child(
            current
                .as_deref()
                .map_or("Default effort", effort_label)
                .to_owned(),
        );
        let title = if think {
            title
                .with_animation(
                    "think-title",
                    Animation::new(Duration::from_millis(2400)).repeat(),
                    |el, t| el.text_color(ultra_hue(t)),
                )
                .into_any_element()
        } else if code {
            title.text_color(pink).into_any_element()
        } else {
            title.into_any_element()
        };

        let header = h_flex()
            .gap_2p5()
            .child(provider_icon(provider, 20.))
            .child(
                v_flex().flex_1().min_w_0().child(title).child(
                    div()
                        .id("to-model-list")
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(muted)
                        .cursor_pointer()
                        .child(self.model_label())
                        .child(Icon::new(IconName::ChevronRight).xsmall())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model_list = true;
                            cx.notify();
                        })),
                ),
            )
            .when(claude, |el| {
                let fast = self.settings.fast;
                el.child(
                    Button::new("fast-bolt")
                        .ghost()
                        .small()
                        .icon(Icon::new(Lucide::Zap).text_color(if fast { warning } else { muted }))
                        .selected(fast)
                        .tooltip("Fast mode")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let settings = TurnSettings {
                                fast: !this.settings.fast,
                                ..this.settings.clone()
                            };
                            this.set_settings(settings, cx);
                        })),
                )
            });

        let slider = (!efforts.is_empty()).then(|| {
            let n = efforts.len();
            // The fill runs from the left edge to the middle of the chosen stop.
            let fill = selected.map(|s| {
                let fill = div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .bottom_0()
                    .w(relative((s as f32 + 0.5) / n as f32))
                    .rounded_full();
                if think {
                    fill.with_animation(
                        "effort-flow",
                        Animation::new(Duration::from_millis(2600)).repeat(),
                        |el, t| {
                            el.bg(linear_gradient(
                                90.,
                                linear_color_stop(ultra_hue(t), 0.),
                                linear_color_stop(ultra_hue((t + 0.45) % 1.), 1.),
                            ))
                        },
                    )
                    .into_any_element()
                } else if code {
                    fill.bg(linear_gradient(
                        90.,
                        linear_color_stop(primary, 0.),
                        linear_color_stop(pink, 1.),
                    ))
                    .into_any_element()
                } else {
                    fill.bg(primary).into_any_element()
                }
            });
            let cells = h_flex()
                .absolute()
                .inset_0()
                .children(efforts.iter().enumerate().map(|(i, effort)| {
                    let is_sel = selected == Some(i);
                    let passed = selected.is_some_and(|s| i < s);
                    let pick = Some(effort.clone());
                    let mark: AnyElement = if is_sel {
                        let thumb = div()
                            .w(px(30.))
                            .h(px(22.))
                            .rounded_full()
                            .bg(white())
                            .shadow_sm();
                        if think {
                            thumb
                                .with_animation(
                                    "thumb-glow",
                                    Animation::new(Duration::from_millis(2400)).repeat(),
                                    |el, t| {
                                        el.shadow(vec![BoxShadow {
                                            color: ultra_hue(t).opacity(0.55),
                                            offset: point(px(0.), px(0.)),
                                            blur_radius: px(
                                                10. + 8. * (t * std::f32::consts::TAU).sin().abs()
                                            ),
                                            spread_radius: px(0.),
                                            inset: false,
                                        }])
                                    },
                                )
                                .into_any_element()
                        } else {
                            thumb.into_any_element()
                        }
                    } else {
                        let color = match effort.as_str() {
                            ULTRATHINK => ultra_hue(0.),
                            ULTRACODE => pink,
                            _ if passed => white().opacity(0.75),
                            _ => fg.opacity(0.3),
                        };
                        div()
                            .size(px(4.))
                            .rounded_full()
                            .bg(color)
                            .into_any_element()
                    };
                    div()
                        .id(("effort", i))
                        .flex_1()
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child(mark)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let settings = TurnSettings {
                                effort: pick.clone(),
                                ..this.settings.clone()
                            };
                            this.set_settings(settings, cx);
                        }))
                }));
            let track = div()
                .relative()
                .h(px(28.))
                .rounded_full()
                .bg(fg.opacity(0.07))
                .children(fill)
                .child(cells);
            let labels = h_flex().children(efforts.iter().enumerate().map(|(i, effort)| {
                let is_sel = selected == Some(i);
                div()
                    .flex_1()
                    .text_center()
                    .text_xs()
                    .text_color(if is_sel {
                        fg
                    } else if effort == ULTRATHINK {
                        ultra_hue(0.)
                    } else if effort == ULTRACODE {
                        pink
                    } else {
                        muted
                    })
                    .when(is_sel, |el| el.font_weight(FontWeight::SEMIBOLD))
                    .child(effort_short(effort).to_owned())
            }));
            v_flex().gap_1p5().child(track).child(labels)
        });

        let note = if think {
            Some((
                "Thinks as long as it needs before it answers. Slower, and uses more tokens.",
                ultra_hue(0.),
            ))
        } else if code {
            Some((
                "Extra high effort, and the agent may split the work across parallel subagents. Uses far more tokens.",
                pink,
            ))
        } else {
            None
        };

        let has_model = self.settings.model.is_some();
        let long = self.settings.long_context;
        v_flex()
            .gap_3()
            .child(header)
            .children(slider)
            .when(efforts.is_empty(), |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("This CLI picks its own reasoning effort."),
                )
            })
            .when_some(note, |el, (text, color)| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(color.opacity(0.08))
                        .border_1()
                        .border_color(color.opacity(0.22))
                        .text_xs()
                        .text_color(muted)
                        .child(text),
                )
            })
            .when(claude, |el| {
                el.child(div().h(px(1.)).bg(fg.opacity(0.08)))
                    .child(
                        h_flex()
                            .gap_2p5()
                            .child(
                                Icon::new(Lucide::Zap)
                                    .small()
                                    .text_color(if self.settings.fast { warning } else { muted }),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Fast mode"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child("Same model, quicker output, higher cost"),
                                    ),
                            )
                            .child(
                                Switch::new("fast-switch")
                                    .checked(self.settings.fast)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        let settings = TurnSettings {
                                            fast: *checked,
                                            ..this.settings.clone()
                                        };
                                        this.set_settings(settings, cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2p5()
                            .child(Icon::new(Lucide::TextAlignStart).small().text_color(muted))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Context window"),
                                    )
                                    .child(div().text_xs().text_color(muted).child(if has_model {
                                        "How much it can hold at once"
                                    } else {
                                        "Pick a model to choose"
                                    })),
                            )
                            .child(
                                segmented(cx)
                                    .child(
                                        segment(cx, "ctx-200k", "200K", !long)
                                            .disabled(!has_model)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let settings = TurnSettings {
                                                    long_context: false,
                                                    ..this.settings.clone()
                                                };
                                                this.set_settings(settings, cx);
                                            })),
                                    )
                                    .child(
                                        segment(cx, "ctx-1m", "1M", long)
                                            .disabled(!has_model)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let settings = TurnSettings {
                                                    long_context: true,
                                                    ..this.settings.clone()
                                                };
                                                this.set_settings(settings, cx);
                                            })),
                                    ),
                            ),
                    )
            })
            .into_any_element()
    }

    /// Search, provider tabs and the models of the chosen tab.
    fn render_model_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let current = self.provider();
        // A thread keeps its CLI once it has messages.
        let locked = !self.transcript.items.is_empty();
        let query = self.model_search.read(cx).value().trim().to_lowercase();
        let rail_pick = self.rail;
        let enabled: Vec<Provider> = if self.look.enabled.is_empty() {
            Provider::ALL.to_vec()
        } else {
            self.look.enabled.clone()
        };

        let mut tabs = h_flex().gap_0p5().flex_wrap().child(
            Button::new("rail-favorites")
                .ghost()
                .xsmall()
                .icon(IconName::Star)
                .selected(rail_pick.is_none())
                .tooltip("Favorites")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.rail = None;
                    cx.notify();
                })),
        );
        for provider in enabled.iter().copied() {
            tabs = tabs.child(
                Button::new(("rail", provider as usize))
                    .ghost()
                    .xsmall()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(provider_icon(provider, 12.))
                            .child(provider.label()),
                    )
                    .selected(rail_pick == Some(provider))
                    .disabled(locked && provider != current)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.rail = Some(provider);
                        if !this.catalog.contains_key(&provider) {
                            this.request(Request::ListModels { provider });
                        }
                        cx.notify();
                    })),
            );
        }

        // (provider, model id or None for the CLI default, label, description)
        let mut entries: Vec<(Provider, Option<String>, String, String)> = Vec::new();
        let providers: Vec<Provider> = match (query.is_empty(), rail_pick) {
            (false, _) => enabled
                .iter()
                .copied()
                .filter(|p| !locked || *p == current)
                .collect(),
            (true, Some(provider)) => vec![provider],
            (true, None) => enabled.clone(),
        };
        for provider in providers {
            let models = self.catalog.get(&provider).cloned().unwrap_or_default();
            let mut list = vec![(
                provider,
                None,
                "Default".to_owned(),
                "The CLI's own default".to_owned(),
            )];
            list.extend(
                models
                    .into_iter()
                    .map(|m| (provider, Some(m.id), m.label, m.description)),
            );
            for entry in list {
                let favorite = entry.1.as_ref().is_some_and(|id| {
                    self.favorites
                        .iter()
                        .any(|(p, m)| *p == provider && m == id)
                });
                let show = match (query.is_empty(), rail_pick) {
                    (false, _) => format!("{} {} {}", entry.2, entry.3, provider.label())
                        .to_lowercase()
                        .contains(&query),
                    (true, None) => favorite,
                    (true, Some(_)) => true,
                };
                if show {
                    entries.push(entry);
                }
            }
        }

        let selected_model = self.settings.model.clone();
        let mut list = v_flex()
            .id("model-list")
            .max_h(px(300.))
            .overflow_y_scroll()
            .gap_0p5();
        if entries.is_empty() {
            list = list.child(div().p_2().text_xs().text_color(muted).child(
                if rail_pick.is_none() && query.is_empty() {
                    "Star models to keep them here."
                } else {
                    "No models match."
                },
            ));
        }
        for (ix, (provider, id, label, description)) in entries.into_iter().enumerate() {
            let selected = provider == current && id == selected_model;
            let favorite = id
                .as_ref()
                .is_some_and(|m| self.favorites.iter().any(|(p, f)| *p == provider && f == m));
            let pick_id = id.clone();
            let mut row = h_flex()
                .id(("model-row", ix))
                .gap_2p5()
                .px_2()
                .py_1p5()
                .rounded_lg()
                .cursor_pointer()
                .when(selected, |el| el.bg(fg.opacity(0.08)))
                .hover(move |style| style.bg(fg.opacity(0.06)))
                .when(locked && provider != current, |el| el.opacity(0.5))
                .child(provider_icon(provider, 15.))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
                        .child(div().text_xs().text_color(muted).child(description)),
                )
                .when(selected, |el| el.child(Icon::new(IconName::Check).small()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if provider != this.provider() {
                        if !this.transcript.items.is_empty() {
                            return;
                        }
                        this.set_provider(provider, cx);
                    }
                    let settings = TurnSettings {
                        model: pick_id.clone(),
                        effort: None,
                        long_context: pick_id.is_some() && this.settings.long_context,
                        ..this.settings.clone()
                    };
                    this.model_list = false;
                    this.set_settings(settings, cx);
                }));
            if let Some(model) = id {
                row = row.child(
                    Button::new(("star", ix))
                        .ghost()
                        .xsmall()
                        .icon(if favorite {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // The star sits on the row; don't also pick the model.
                            cx.stop_propagation();
                            this.request(Request::ToggleFavorite {
                                provider,
                                model: model.clone(),
                            })
                        })),
                );
            }
            list = list.child(row);
        }

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("back-to-options")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronLeft)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model_list = false;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().child(Input::new(&self.model_search).small())),
            )
            .child(tabs)
            .child(list)
            .into_any_element()
    }

    fn render_access_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let current = self.settings.access;
        let mut list = v_flex().gap_0p5();
        for access in Access::ALL {
            list = list.child(
                v_flex()
                    .id(("access", access as usize))
                    .px_2p5()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .when(access == current, |el| el.bg(fg.opacity(0.08)))
                    .hover(move |style| style.bg(fg.opacity(0.06)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(access_color(access, cx)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(access.label()),
                            )
                            .when(access == current, |el| {
                                el.child(Icon::new(IconName::Check).small())
                            }),
                    )
                    .child(
                        div()
                            .pl(px(14.))
                            .text_xs()
                            .text_color(muted)
                            .child(access.description()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let settings = TurnSettings {
                            access,
                            ..this.settings.clone()
                        };
                        this.picker = None;
                        this.set_settings(settings, cx);
                    })),
            );
        }
        Popover::new("access-picker")
            .anchor(Anchor::BottomLeft)
            .open(self.picker == Some(Picker::Access))
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.picker = open.then_some(Picker::Access);
                cx.notify();
            }))
            .trigger(
                Button::new("access-trigger")
                    .ghost()
                    .xsmall()
                    .icon(Lucide::Lock)
                    .label(current.label())
                    .text_color(muted),
            )
            .w(px(300.))
            .p_1()
            .bg(theme.popover.opacity(0.94))
            .child(list)
            .into_any_element()
    }

    /// The draft's project chip: which folder a new Code thread starts in.
    fn render_project_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let current = self.project();
        let name = current
            .and_then(|id| self.look.projects.iter().find(|(p, _)| *p == id))
            .map_or("No project".to_owned(), |(_, n)| n.clone());
        let mut list = v_flex().gap_0p5();
        let options = std::iter::once((None, "No project".to_owned())).chain(
            self.look
                .projects
                .iter()
                .map(|(id, name)| (Some(*id), name.clone())),
        );
        for (ix, (id, label)) in options.enumerate() {
            list = list.child(
                h_flex()
                    .id(("draft-project", ix))
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(id == current, |el| el.bg(fg.opacity(0.08)))
                    .hover(move |style| style.bg(fg.opacity(0.06)))
                    .child(Icon::new(Lucide::Folder).small())
                    .child(div().flex_1().text_sm().child(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.picker = None;
                        this.set_project(id, cx);
                    })),
            );
        }
        Popover::new("project-picker")
            .anchor(Anchor::BottomRight)
            .open(self.picker == Some(Picker::Project))
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.picker = open.then_some(Picker::Project);
                cx.notify();
            }))
            .trigger(
                Button::new("project-trigger")
                    .ghost()
                    .xsmall()
                    .icon(Lucide::Folder)
                    .label(name),
            )
            .w(px(240.))
            .p_1()
            .bg(theme.popover.opacity(0.94))
            .child(list)
            .into_any_element()
    }

    /// Commands matching what follows a leading `/`.
    fn slash_matches(&self, cx: &App) -> Vec<String> {
        let value = self.composer.read(cx).value();
        let Some(query) = value.strip_prefix('/') else {
            return Vec::new();
        };
        if query.contains(char::is_whitespace) {
            return Vec::new();
        }
        let query = query.to_lowercase();
        self.commands
            .get(&self.provider())
            .map(|names| {
                names
                    .iter()
                    .filter(|n| n.to_lowercase().contains(&query))
                    .take(MAX_COMMANDS)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn render_composer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.running();
        let chat = self.is_chat();
        let draft = self.is_draft();
        let queued = self.info.as_ref().map_or(0, |info| info.queued);
        let pending = self.pending().map(|ix| self.render_pending(ix, cx));
        let model = self.render_model_picker(cx);
        let access = (!chat).then(|| self.render_access_picker(cx));
        let project = (draft && !chat).then(|| self.render_project_picker(cx));
        let commands = self.slash_matches(cx);
        let theme = cx.theme();
        let (fg, muted, primary, background) = (
            theme.foreground,
            theme.muted_foreground,
            theme.primary,
            theme.background,
        );
        let mono = theme.mono_font_family.clone();
        let plan = self.settings.mode == Mode::Plan;
        let thread = self.id;

        let slash = (!commands.is_empty()).then(|| {
            v_flex()
                .gap_0p5()
                .children(commands.into_iter().enumerate().map(|(ix, name)| {
                    let insert = format!("/{name} ");
                    h_flex()
                        .id(("slash", ix))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(move |style| style.bg(fg.opacity(0.06)))
                        .text_sm()
                        .font_family(mono.clone())
                        .child(format!("/{name}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let insert = insert.clone();
                            this.composer
                                .update(cx, |composer, cx| composer.set_value(insert, window, cx));
                        }))
                }))
        });

        let actions = if running {
            h_flex()
                .gap_1()
                .when(queued > 0, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .mr_1()
                            .child(format!("{queued} queued")),
                    )
                })
                .when(!chat, |el| {
                    el.child(
                        Button::new("steer")
                            .outline()
                            .small()
                            .label("Steer")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send(Delivery::SteerNow, window, cx)
                            })),
                    )
                })
                .child(model)
                .child(
                    Button::new("stop")
                        .small()
                        .rounded_full()
                        .icon(IconName::Square)
                        .tooltip("Stop")
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.request(Request::Interrupt { thread })
                        })),
                )
        } else {
            h_flex().gap_1().child(model).child(
                Button::new("send")
                    .primary()
                    .small()
                    .rounded_full()
                    .icon(IconName::ArrowUp)
                    .tooltip("Send")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.send(Delivery::Queue, window, cx)),
                    ),
            )
        };

        let boxed = v_flex()
            .relative()
            .w_full()
            .gap_3()
            .pt_3()
            .pb_2p5()
            .pl_4()
            .pr_3()
            .rounded(px(18.))
            .bg(background.opacity(0.66))
            .border_1()
            .border_color(fg.opacity(0.1))
            .shadow_lg()
            .children(pending)
            .children(slash)
            .child(
                Textarea::new(&self.composer)
                    .appearance(false)
                    .bordered(false),
            )
            .child(h_flex().items_center().child(div().flex_1()).child(actions))
            // A beam rides the border while the agent works (libraries.dev's border beam, drawn natively).
            .when(running, |el| {
                el.child(style::border_beam("composer-beam", primary))
            });

        let under = (!chat).then(|| {
            h_flex()
                .gap_1()
                .px_1p5()
                .child(
                    segmented(cx)
                        .child(
                            segment(cx, "mode-build", "Build", !plan).on_click(cx.listener(
                                |this, _, _, cx| {
                                    let settings = TurnSettings {
                                        mode: Mode::Agent,
                                        ..this.settings.clone()
                                    };
                                    this.set_settings(settings, cx);
                                },
                            )),
                        )
                        .child(segment(cx, "mode-plan", "Plan", plan).on_click(cx.listener(
                            |this, _, _, cx| {
                                let settings = TurnSettings {
                                    mode: Mode::Plan,
                                    ..this.settings.clone()
                                };
                                this.set_settings(settings, cx);
                            },
                        ))),
                )
                .children(access)
                .child(div().flex_1())
        });

        let column = self.column();
        div()
            .w_full()
            .flex()
            .justify_center()
            .px_6()
            .pb_4()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(column))
                    .gap_2()
                    .when_some(project, |el, project| {
                        el.child(h_flex().justify_end().px_1p5().child(project))
                    })
                    .child(boxed)
                    .children(under),
            )
            .into_any_element()
    }

    fn render_pane(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (fg, muted, background) = (theme.foreground, theme.muted_foreground, theme.background);
        let thread = self.id;
        if matches!(self.pane, Pane::None) {
            return None;
        }
        let on_diff = matches!(self.pane, Pane::Diff(..));
        let tabs = h_flex()
            .gap_1()
            .child(
                Button::new("tab-files")
                    .ghost()
                    .xsmall()
                    .label("Files")
                    .selected(!on_diff)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::ListFiles { thread })
                    })),
            )
            .when(on_diff, |el| {
                el.child(
                    Button::new("tab-diff")
                        .ghost()
                        .xsmall()
                        .label("Changes")
                        .selected(true),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("refresh-files")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::ListFiles { thread })
                    })),
            )
            .child(
                Button::new("close-pane")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pane = Pane::None;
                        cx.notify();
                    })),
            );
        let pane = v_flex()
            .id("pane")
            .w(px(420.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(fg.opacity(0.07))
            .bg(background.opacity(0.5))
            .p_2()
            .gap_2()
            .overflow_y_scroll()
            .child(tabs);
        Some(match &self.pane {
            Pane::None => unreachable!(),
            Pane::Diff(turn, text) => pane
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("Turn {turn}")),
                )
                .child(TextView::markdown(("diff", *turn as usize), text.clone()))
                .into_any_element(),
            Pane::Files => {
                let list = v_flex()
                    .gap_0p5()
                    .children(self.files.iter().enumerate().map(|(ix, file)| {
                        let path = file.path.clone();
                        let depth = path.matches('/').count();
                        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
                        let label = if file.is_dir {
                            format!("{name}/")
                        } else {
                            name
                        };
                        Button::new(("file", ix))
                            .ghost()
                            .xsmall()
                            .label(label)
                            .disabled(file.is_dir)
                            .ml(px(12. * depth as f32))
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::ReadFile {
                                    thread,
                                    path: path.clone(),
                                })
                            }))
                    }));
                let viewer = self.file.as_ref().map(|(path, content, text)| {
                    let header = div().text_xs().text_color(muted).child(path.clone());
                    let body = match content {
                        FileContent::Markdown(_) | FileContent::Text { .. } => {
                            TextView::markdown("file-view", text.clone()).into_any_element()
                        }
                        FileContent::Image(file) => {
                            img(file.clone()).max_w_full().into_any_element()
                        }
                        FileContent::Html(file) | FileContent::Binary(file) => {
                            let url = file_url(file);
                            let label = if matches!(content, FileContent::Html(_)) {
                                "Open in browser"
                            } else {
                                "Open with the default app"
                            };
                            Button::new("open-file")
                                .small()
                                .label(label)
                                .on_click(move |_, _, cx| cx.open_url(&url))
                                .into_any_element()
                        }
                    };
                    v_flex()
                        .gap_1()
                        .border_t_1()
                        .border_color(fg.opacity(0.07))
                        .pt_2()
                        .child(header)
                        .child(body)
                });
                pane.child(list).children(viewer).into_any_element()
            }
        })
    }

    /// The new-thread screen's heading, when no wallpaper fills the space.
    fn render_hero(&self, cx: &App) -> Option<AnyElement> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        if self.is_chat() {
            return Some(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::MEDIUM)
                    .child(greeting())
                    .into_any_element(),
            );
        }
        if self.look.wallpaper {
            return None;
        }
        let project = self
            .project()
            .and_then(|id| self.look.projects.iter().find(|(p, _)| *p == id))
            .map(|(_, n)| n.clone());
        Some(
            v_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_3xl()
                        .font_weight(FontWeight::MEDIUM)
                        .child(match &project {
                            Some(name) => format!("What should we build in {name}?"),
                            None => "What should we work on?".to_owned(),
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(if project.is_some() {
                            "Runs in the project folder, with checkpoints you can undo."
                        } else {
                            "Pick a project above, or start in a scratch folder."
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Render for ThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let thread = self.id;
        let empty = self.transcript.items.is_empty();
        let hero = empty.then(|| self.render_hero(cx)).flatten();
        let composer = self.render_composer(cx);
        let pane = self.render_pane(cx);

        let center = if empty {
            v_flex()
                .flex_1()
                .min_h_0()
                .justify_center()
                .pb(px(80.))
                .gap_6()
                .when_some(hero, |el, hero| {
                    el.child(h_flex().justify_center().px_6().child(hero))
                })
                .child(composer)
        } else {
            let transcript = MessageScroller::new(
                "transcript",
                self.scroller.clone(),
                cx.processor(Self::render_row),
            )
            .flex_1()
            .min_h_0();
            v_flex()
                .flex_1()
                .min_h_0()
                .when(self.older.is_some(), |el| {
                    el.child(
                        h_flex().justify_center().pt_2().child(
                            Button::new("older")
                                .ghost()
                                .xsmall()
                                .label("Load older messages")
                                .on_click(cx.listener(move |this, _, _, _| {
                                    if let Some(before) = this.older.take() {
                                        this.request(Request::LoadOlder { id: thread, before });
                                    }
                                })),
                        ),
                    )
                })
                .child(transcript)
                .child(composer)
        };

        h_flex()
            .size_full()
            .child(v_flex().flex_1().min_w_0().h_full().child(center))
            .children(pane)
    }
}

/// The new-chat heading. Not "Good morning": std has no local time zone,
/// and a greeting that is wrong half the day is worse than none.
fn greeting() -> &'static str {
    "What can I help with?"
}

fn access_color(access: Access, cx: &App) -> Hsla {
    let theme = cx.theme();
    match access {
        Access::Supervised => theme.muted_foreground,
        Access::AutoEdits => theme.info,
        Access::Auto => theme.primary,
        Access::FullAccess => theme.danger,
    }
}

pub fn effort_label(effort: &str) -> &str {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        ULTRATHINK => "Ultrathink",
        ULTRACODE => "Ultracode",
        other => other,
    }
}

fn effort_short(effort: &str) -> &str {
    match effort {
        "medium" => "Med",
        "xhigh" => "XHigh",
        ULTRATHINK => "Think",
        ULTRACODE => "Code",
        other => effort_label(other),
    }
}

/// 12400 as "12.4k".
fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "Read",
        ToolKind::Edit => "Edit",
        ToolKind::Execute => "Run",
        ToolKind::Search => "Search",
        ToolKind::Fetch => "Fetch",
        ToolKind::Think => "Think",
        ToolKind::Other => "Tool",
    }
}

fn status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "failed",
    }
}

fn resolved_label(choice: PermChoice, plan: bool) -> &'static str {
    match (choice, plan) {
        (PermChoice::Deny, true) => "Sent back to refine",
        (_, true) => "Approved",
        (PermChoice::AllowOnce, false) => "Approved",
        (PermChoice::AllowAlways, false) => "Always allowed",
        (PermChoice::Deny, false) => "Declined",
    }
}

fn reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "Done",
        StopReason::MaxTokens => "Hit the output limit",
        StopReason::Interrupted => "Stopped",
        StopReason::Error => "Failed",
        StopReason::Timeout => "Timed out",
    }
}

/// A `file://` URL the system browser or default app can open.
pub fn file_url(path: &std::path::Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's glob exports a `test` attribute that shadows std's.
    use super::{Row, build_rows, tokens};
    use proto::{Item, ToolKind, ToolStatus};

    fn tool(id: &str) -> Item {
        Item::Tool {
            call_id: id.into(),
            kind: ToolKind::Read,
            title: id.into(),
            status: ToolStatus::Done,
            preview: String::new(),
            output: None,
        }
    }

    #[test]
    fn tool_runs_fold_into_one_row() {
        let items = vec![
            Item::User {
                text: "hi".into(),
                turn: 1,
            },
            tool("a"),
            Item::Thinking {
                msg_id: "m".into(),
                text: "…".into(),
            },
            tool("b"),
            Item::Assistant {
                msg_id: "m".into(),
                text: "done".into(),
            },
            tool("c"),
        ];
        assert_eq!(
            build_rows(&items, 0),
            vec![
                Row::Item(0),
                Row::Work { start: 1, end: 4 },
                Row::Item(4),
                Row::Work { start: 5, end: 6 }
            ]
        );
        assert_eq!(
            build_rows(&items, 4),
            vec![Row::Item(4), Row::Work { start: 5, end: 6 }]
        );
    }

    #[test]
    fn token_counts_read_short() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_400), "12.4k");
        assert_eq!(tokens(3_610_000), "3.6M");
    }
}

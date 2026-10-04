//! One open thread, laid out like T3 Code: a centered timeline where tool
//! activity folds into one "worked" row per stretch, and a composer that holds
//! every control — model, reasoning effort, Build/Plan, access mode, send,
//! queue, steer and stop — plus the approval or question waiting on the user.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    message_scroller::{MessageScroller, MessageScrollerState},
    popover::Popover,
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    Access, AgentEvent, BlobRef, Delivery, FileContent, FileEntry, Item, Mode, ModelInfo,
    PermChoice, Provider, Request, Seq, StopReason, ThreadEvent, ThreadId, ThreadInfo, TodoStatus,
    ToolKind, ToolStatus, Transcript, TurnSettings,
};
use tokio::sync::mpsc;

/// Width of the timeline and composer column.
const COLUMN: f32 = 820.;
/// Full tool outputs kept after "show all"; older ones are dropped.
const MAX_EXPANDED: usize = 8;

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
    Effort,
    Access,
}

enum Pane {
    None,
    Files,
    Diff(u32, SharedString),
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
    picker: Option<Picker>,
    model_search: Entity<InputState>,
    /// The picker's provider rail; `None` shows favorites.
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
                .placeholder("Ask anything, @ to mention files")
        });
        let model_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search models"));
        // Not focused on open: a focused composer blinks its caret, which redraws an idle window.
        let _subscriptions = vec![
            cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.send(Delivery::Queue, window, cx);
                }
            }),
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
            picker: None,
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

    pub fn set_catalog(
        &mut self,
        catalog: HashMap<Provider, Vec<ModelInfo>>,
        favorites: Vec<(Provider, String)>,
        cx: &mut Context<Self>,
    ) {
        self.catalog = catalog;
        self.favorites = favorites;
        cx.notify();
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

    fn set_settings(&mut self, settings: TurnSettings, cx: &mut Context<Self>) {
        self.settings = settings;
        self.request(Request::SetThreadSettings {
            thread: self.id,
            settings: self.settings.clone(),
        });
        cx.notify();
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

    fn render_row(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(row_ix).copied() else {
            return div().into_any_element();
        };
        let body = match row {
            Row::Item(ix) => self.render_item(ix, cx),
            Row::Work { start, end } => self.render_work(start, end, cx),
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px_4()
            .py_1p5()
            .child(div().w_full().max_w(px(COLUMN)).child(body))
            .into_any_element()
    }

    fn render_item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.transcript.items.get(ix).cloned() else {
            return div().into_any_element();
        };
        let text = self.text(ix);
        let theme = cx.theme();
        let (border, muted, danger, secondary) = (
            theme.border,
            theme.muted_foreground,
            theme.danger,
            theme.secondary,
        );
        let thread = self.id;
        let running = self.running();
        match item {
            Item::User { .. } => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.8))
                        .px_4()
                        .py_2()
                        .rounded_2xl()
                        .bg(secondary)
                        .child(text),
                )
                .into_any_element(),
            Item::Assistant { .. } => match &self.live {
                Some((live_ix, state)) if *live_ix == ix => TextView::new(state).into_any_element(),
                _ => TextView::markdown(("md", ix), text).into_any_element(),
            },
            Item::Permission {
                title,
                plan: true,
                resolved,
                ..
            } => v_flex()
                .gap_2()
                .p_4()
                .rounded_xl()
                .border_1()
                .border_color(border)
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
            Item::Todo { items } => v_flex()
                .gap_1()
                .p_3()
                .rounded_xl()
                .border_1()
                .border_color(border)
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
                .border_color(danger)
                .text_sm()
                .text_color(danger)
                .child(message)
                .into_any_element(),
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
                    "{} · {input} in / {output} out",
                    reason_label(reason)
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
        let (border, muted, danger, mono) = (
            theme.border,
            theme.muted_foreground,
            theme.danger,
            theme.mono_font_family.clone(),
        );
        let open = self.opened.contains(&start);
        let active = self.running() && end == self.transcript.items.len();
        let steps = (start..end)
            .filter(|&ix| matches!(self.transcript.items[ix], Item::Tool { .. }))
            .count();
        let label = format!(
            "{} {} · {steps} step{}",
            if open { "▾" } else { "▸" },
            if active { "Working" } else { "Worked" },
            if steps == 1 { "" } else { "s" }
        );
        let header = div()
            .id(("work", start))
            .text_sm()
            .text_color(muted)
            .cursor_pointer()
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
        if !open {
            return header.into_any_element();
        }
        let mut steps_view = v_flex()
            .gap_2()
            .pl_3()
            .ml_1()
            .border_l_1()
            .border_color(border);
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
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_xs().text_color(muted).child(kind_label(kind)))
                                .child(div().flex_1().text_sm().child(title))
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
                                    .text_xs()
                                    .font_family(mono.clone())
                                    .text_color(muted)
                                    .child(text),
                            )
                        })
                        .when_some(output.filter(|_| !expanded), |el, blob| {
                            el.child(
                                Button::new(("show-all", ix))
                                    .ghost()
                                    .xsmall()
                                    .label("Show all output")
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.request(Request::ReadBlob { blob: blob.clone() })
                                    })),
                            )
                        })
                        .into_any_element()
                }
                _ => div().into_any_element(),
            };
            steps_view = steps_view.child(step);
        }
        v_flex()
            .gap_2()
            .child(header)
            .child(steps_view)
            .into_any_element()
    }

    /// The approval or question panel shown inside the composer.
    fn render_pending(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, secondary) = (theme.muted_foreground, theme.secondary);
        let thread = self.id;
        let detail = self.text(ix);
        let panel = v_flex().gap_2().p_3().rounded_xl().bg(secondary);
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
                            .ghost(),
                        )
                        .child(choice("decline", "Decline", PermChoice::Deny).ghost())
                };
                panel
                    .child(div().text_xs().text_color(muted).child(kind))
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
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(question.header.to_uppercase()),
                            )
                            .when(count > 1, |el| {
                                el.child(
                                    div()
                                        .text_xs()
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

    fn model_label(&self) -> String {
        let provider = self.provider();
        match &self.settings.model {
            None => format!("{} · Default", provider.label()),
            Some(id) => {
                let label = self
                    .catalog
                    .get(&provider)
                    .and_then(|models| models.iter().find(|m| m.id == *id))
                    .map_or(id.clone(), |m| m.label.clone());
                format!("{} · {label}", provider.label())
            }
        }
    }

    fn render_model_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, secondary) = (theme.muted_foreground, theme.secondary);
        let current = self.provider();
        // A thread keeps its CLI once it has messages.
        let locked = !self.transcript.items.is_empty();
        let query = self.model_search.read(cx).value().trim().to_lowercase();
        let rail_pick = self.rail;

        let mut rail = v_flex().w(px(130.)).gap_0p5().child(
            Button::new("rail-favorites")
                .ghost()
                .xsmall()
                .w_full()
                .icon(IconName::Star)
                .label("Favorites")
                .selected(rail_pick.is_none())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.rail = None;
                    cx.notify();
                })),
        );
        for provider in Provider::ALL {
            rail = rail.child(
                Button::new(("rail", provider as usize))
                    .ghost()
                    .xsmall()
                    .w_full()
                    .label(provider.label())
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
            (false, _) => Provider::ALL
                .into_iter()
                .filter(|p| !locked || *p == current)
                .collect(),
            (true, Some(provider)) => vec![provider],
            (true, None) => Provider::ALL.to_vec(),
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
            .flex_1()
            .h_full()
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
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(move |style| style.bg(secondary))
                .when(locked && provider != current, |el| el.opacity(0.5))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(label))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{} · {description}", provider.label())),
                        ),
                )
                .when(selected, |el| el.child(Icon::new(IconName::Check).small()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if provider != this.provider() {
                        if !this.transcript.items.is_empty() {
                            return;
                        }
                        this.request(Request::SetThreadProvider {
                            thread: this.id,
                            provider,
                        });
                    }
                    let settings = TurnSettings {
                        model: pick_id.clone(),
                        effort: None,
                        ..this.settings.clone()
                    };
                    this.picker = None;
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

        let open = self.picker == Some(Picker::Model);
        let label = self.model_label();
        Popover::new("model-picker")
            .anchor(Anchor::BottomLeft)
            .open(open)
            .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                this.picker = open.then_some(Picker::Model);
                let provider = this.provider();
                if *open && !this.catalog.contains_key(&provider) {
                    this.request(Request::ListModels { provider });
                }
                cx.notify();
            }))
            .trigger(
                Button::new("model-trigger")
                    .ghost()
                    .xsmall()
                    .label(label)
                    .icon(IconName::ChevronDown),
            )
            .w(px(480.))
            .h(px(380.))
            .p_2()
            .gap_2()
            .child(Input::new(&self.model_search).small())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_2()
                    .child(rail)
                    .child(list),
            )
            .into_any_element()
    }

    fn render_effort_picker(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let provider = self.provider();
        let efforts = match &self.settings.model {
            Some(id) => self
                .catalog
                .get(&provider)
                .and_then(|models| models.iter().find(|m| m.id == *id))
                .map(|m| m.efforts.clone())
                .unwrap_or_default(),
            None => self
                .catalog
                .get(&provider)
                .and_then(|models| models.first())
                .map(|m| m.efforts.clone())
                .unwrap_or_default(),
        };
        if efforts.is_empty() {
            return None;
        }
        let current = self.settings.effort.clone();
        let mut list = v_flex().gap_0p5();
        for (ix, effort) in std::iter::once(None)
            .chain(efforts.into_iter().map(Some))
            .enumerate()
        {
            let label = effort
                .clone()
                .map_or("Default".to_owned(), |e| capitalize(&e));
            let pick = effort.clone();
            list = list.child(
                Button::new(("effort", ix))
                    .ghost()
                    .xsmall()
                    .w_full()
                    .label(label)
                    .selected(current == effort)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let settings = TurnSettings {
                            effort: pick.clone(),
                            ..this.settings.clone()
                        };
                        this.picker = None;
                        this.set_settings(settings, cx);
                    })),
            );
        }
        let label = current.map_or("Effort".to_owned(), |e| capitalize(&e));
        Some(
            Popover::new("effort-picker")
                .anchor(Anchor::BottomLeft)
                .open(self.picker == Some(Picker::Effort))
                .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                    this.picker = open.then_some(Picker::Effort);
                    cx.notify();
                }))
                .trigger(
                    Button::new("effort-trigger")
                        .ghost()
                        .xsmall()
                        .label(label)
                        .icon(IconName::ChevronDown),
                )
                .w(px(180.))
                .p_1()
                .child(list)
                .into_any_element(),
        )
    }

    fn render_access_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let current = self.settings.access;
        let mut list = v_flex().gap_1();
        for access in Access::ALL {
            list = list.child(
                div()
                    .id(("access", access as usize))
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(access.label()))
                            .when(access == current, |el| {
                                el.child(Icon::new(IconName::Check).small())
                            }),
                    )
                    .child(
                        div()
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
                    .label(current.label())
                    .icon(IconName::ChevronDown),
            )
            .w(px(300.))
            .p_1()
            .child(list)
            .into_any_element()
    }

    fn render_composer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.running();
        let queued = self.info.as_ref().map_or(0, |info| info.queued);
        let pending = self.pending().map(|ix| self.render_pending(ix, cx));
        let model = self.render_model_picker(cx);
        let effort = self.render_effort_picker(cx);
        let access = self.render_access_picker(cx);
        let theme = cx.theme();
        let (border, background, muted) = (theme.border, theme.background, theme.muted_foreground);
        let plan = self.settings.mode == Mode::Plan;
        let thread = self.id;

        let mode = Button::new("mode-toggle")
            .ghost()
            .xsmall()
            .icon(if plan { IconName::Map } else { IconName::Bot })
            .label(if plan { "Plan" } else { "Build" })
            .selected(plan)
            .on_click(cx.listener(|this, _, _, cx| {
                let mode = if this.settings.mode == Mode::Plan {
                    Mode::Agent
                } else {
                    Mode::Plan
                };
                let settings = TurnSettings {
                    mode,
                    ..this.settings.clone()
                };
                this.set_settings(settings, cx);
            }));

        let actions = if running {
            h_flex()
                .gap_1()
                .when(queued > 0, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("{queued} queued")),
                    )
                })
                .child(
                    Button::new("steer")
                        .ghost()
                        .xsmall()
                        .label("Steer")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.send(Delivery::SteerNow, window, cx)
                        })),
                )
                .child(Button::new("queue").xsmall().label("Queue").on_click(
                    cx.listener(|this, _, window, cx| this.send(Delivery::Queue, window, cx)),
                ))
                .child(
                    Button::new("stop")
                        .danger()
                        .xsmall()
                        .icon(IconName::Square)
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.request(Request::Interrupt { thread })
                        })),
                )
        } else {
            h_flex().child(
                Button::new("send")
                    .primary()
                    .small()
                    .icon(IconName::ArrowUp)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.send(Delivery::Queue, window, cx)),
                    ),
            )
        };

        div()
            .w_full()
            .flex()
            .justify_center()
            .px_4()
            .pb_4()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(COLUMN))
                    .gap_2()
                    .p_2()
                    .rounded_2xl()
                    .border_1()
                    .border_color(border)
                    .bg(background)
                    .children(pending)
                    .child(Textarea::new(&self.composer).bordered(false))
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(model)
                            .children(effort)
                            .child(mode)
                            .child(access)
                            .child(div().flex_1())
                            .child(actions),
                    ),
            )
            .into_any_element()
    }

    fn render_pane(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let thread = self.id;
        let pane = v_flex()
            .id("pane")
            .w(px(380.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(border)
            .p_2()
            .gap_2()
            .overflow_y_scroll();
        let close = Button::new("close-pane")
            .ghost()
            .xsmall()
            .icon(IconName::Close)
            .on_click(cx.listener(|this, _, _, cx| {
                this.pane = Pane::None;
                cx.notify();
            }));
        match &self.pane {
            Pane::None => None,
            Pane::Diff(turn, text) => Some(
                pane.child(
                    h_flex()
                        .justify_between()
                        .child(format!("Changes in turn {turn}"))
                        .child(close),
                )
                .child(TextView::markdown(("diff", *turn as usize), text.clone()))
                .into_any_element(),
            ),
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
                        .border_color(border)
                        .pt_2()
                        .child(header)
                        .child(body)
                });
                Some(
                    pane.child(
                        h_flex().justify_between().child("Files").child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("refresh-files")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::RefreshCw)
                                        .on_click(cx.listener(move |this, _, _, _| {
                                            this.request(Request::ListFiles { thread })
                                        })),
                                )
                                .child(close),
                        ),
                    )
                    .child(list)
                    .children(viewer)
                    .into_any_element(),
                )
            }
        }
    }
}

impl Render for ThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let thread = self.id;
        let running = self.running();
        let needs_input = self.info.as_ref().is_some_and(|info| info.needs_input);
        let title = self
            .info
            .as_ref()
            .map(|info| info.title.clone())
            .unwrap_or_default();
        let folder = self
            .info
            .as_ref()
            .and_then(|info| {
                // A chat's scratch folder is named after its id; only a project folder means anything.
                info.project?;
                info.folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let empty = self.transcript.items.is_empty();
        let composer = self.render_composer(cx);
        let pane = self.render_pane(cx);
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);

        let status = if needs_input {
            Some("Needs input")
        } else if running {
            Some("Working")
        } else {
            None
        };
        let header = h_flex()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().text_xs().text_color(muted).child(folder.clone()))
            .when_some(status, |el, status| {
                el.child(div().text_xs().text_color(muted).child(status))
            })
            .child(div().flex_1())
            .when(self.older.is_some(), |el| {
                el.child(
                    Button::new("older")
                        .ghost()
                        .xsmall()
                        .label("Load older")
                        .on_click(cx.listener(move |this, _, _, _| {
                            if let Some(before) = this.older.take() {
                                this.request(Request::LoadOlder { id: thread, before });
                            }
                        })),
                )
            })
            .child(
                Button::new("files")
                    .ghost()
                    .xsmall()
                    .icon(IconName::FolderOpen)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::ListFiles { thread })
                    })),
            );

        let center = if empty {
            v_flex()
                .flex_1()
                .min_h_0()
                .justify_center()
                .gap_6()
                .child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .text_2xl()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("What should we work on?"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .when(!folder.is_empty(), |el| el.child(format!("in {folder}"))),
                        ),
                )
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
                .child(transcript)
                .child(composer)
        };

        h_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(header)
                    .child(center),
            )
            .children(pane)
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
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
    use super::{Row, build_rows};
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
}

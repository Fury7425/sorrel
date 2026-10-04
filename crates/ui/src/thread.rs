//! One open thread: the virtualized transcript with its cards, the composer,
//! and the file and diff pane.

use std::collections::HashMap;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    AgentEvent, BlobRef, Delivery, FileContent, FileEntry, Item, Mode, PermChoice, Request, Seq,
    StopReason, ThreadEvent, ThreadId, ThreadInfo, TodoStatus, ToolKind, ToolStatus, Transcript,
};
use tokio::sync::mpsc;

/// Full tool outputs kept after "show all"; older ones are dropped.
const MAX_EXPANDED: usize = 8;

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
    /// Text for each row, built once and shared with the renderer, so
    /// unchanged rows never re-parse.
    cache: Vec<Option<SharedString>>,
    /// The newest assistant row owns markdown state it appends to while it
    /// streams. Every other row uses keyed state that GPUI drops off screen.
    live: Option<(usize, Entity<TextViewState>)>,
    scroller: Entity<MessageScrollerState>,
    older: Option<Seq>,
    composer: Entity<TextareaState>,
    mode: Mode,
    delivery: Delivery,
    expanded: Vec<(String, SharedString)>,
    drafts: HashMap<String, Vec<Vec<String>>>,
    pane: Pane,
    files: Vec<FileEntry>,
    file: Option<(String, FileContent, SharedString)>,
    turns_ended: usize,
    _subscription: Subscription,
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
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Message (Enter sends, Shift+Enter for a new line)")
        });
        composer.update(cx, |composer, cx| composer.focus(window, cx));
        let _subscription = cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.send(window, cx);
            }
        });
        Self {
            id,
            requests,
            info,
            transcript: Transcript::default(),
            cache: Vec::new(),
            live: None,
            scroller,
            older: None,
            composer,
            mode: Mode::Agent,
            delivery: Delivery::Queue,
            expanded: Vec::new(),
            drafts: HashMap::new(),
            pane: Pane::None,
            files: Vec::new(),
            file: None,
            turns_ended: 0,
            _subscription,
        }
    }

    fn request(&self, request: Request) {
        let _ = self.requests.try_send(request);
    }

    fn running(&self) -> bool {
        self.info.as_ref().is_some_and(|info| info.running)
    }

    pub fn set_info(&mut self, info: Option<ThreadInfo>, cx: &mut Context<Self>) {
        if self.info != info {
            self.info = info;
            cx.notify();
        }
    }

    pub fn row_count(&self) -> usize {
        self.transcript.items.len()
    }

    pub fn turns_ended(&self) -> usize {
        self.turns_ended
    }

    pub fn scroll_to_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.scroller.update(cx, |scroller, cx| {
            scroller.scroll_to_item(ix, cx);
        });
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
        if prepend {
            let n = page.items.len();
            self.transcript.prepend(page.items);
            self.cache.splice(0..0, std::iter::repeat_n(None, n));
            if let Some((ix, _)) = &mut self.live {
                *ix += n;
            }
            self.scroller.update(cx, |scroller, cx| {
                scroller.prepend(n, cx);
            });
        } else {
            let n = page.items.len();
            self.transcript = page;
            self.cache = vec![None; n];
            self.live = None;
            self.scroller
                .update(cx, |scroller, cx| scroller.reset(n, cx));
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
            let added = len - self.cache.len();
            self.cache.resize(len, None);
            self.scroller.update(cx, |scroller, cx| {
                scroller.append(added, cx);
            });
        } else {
            self.cache[ix] = None;
            self.scroller.update(cx, |scroller, cx| {
                scroller.remeasure_items(ix..ix + 1, cx);
            });
        }
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

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer
            .update(cx, |composer, cx| composer.set_value("", window, cx));
        self.request(Request::Send {
            thread: self.id,
            text,
            mode: self.mode,
            delivery: self.delivery,
        });
        self.scroller
            .update(cx, |scroller, cx| scroller.scroll_to_end(cx));
    }

    /// The row's text as a shared string, built on first use.
    fn text(&mut self, ix: usize) -> SharedString {
        if let Some(Some(text)) = self.cache.get(ix) {
            return text.clone();
        }
        let text: SharedString = match &self.transcript.items[ix] {
            Item::User { text, .. } | Item::Assistant { text, .. } => text.clone().into(),
            Item::Thinking { text, .. } => {
                let short: String = text.chars().take(400).collect();
                format!(
                    "Thinking: {short}{}",
                    if short.len() < text.len() { "…" } else { "" }
                )
                .into()
            }
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

    fn render_row(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.transcript.items.get(ix).cloned() else {
            return div().into_any_element();
        };
        let text = self.text(ix);
        let theme = cx.theme();
        let (border, muted, danger, secondary, mono) = (
            theme.border,
            theme.muted_foreground,
            theme.danger,
            theme.secondary,
            theme.mono_font_family.clone(),
        );
        let thread = self.id;
        let running = self.running();
        let body = match item {
            Item::User { .. } => v_flex()
                .items_end()
                .gap_1()
                .child(
                    div()
                        .max_w(relative(0.85))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(secondary)
                        .child(text),
                )
                .into_any_element(),
            Item::Assistant { .. } => match &self.live {
                Some((live_ix, state)) if *live_ix == ix => TextView::new(state).into_any_element(),
                _ => TextView::markdown(("md", ix), text).into_any_element(),
            },
            Item::Thinking { .. } => div()
                .text_xs()
                .text_color(muted)
                .child(text)
                .into_any_element(),
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
                    .p_2()
                    .border_1()
                    .border_color(border)
                    .rounded_md()
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
                        el.child(div().text_xs().font_family(mono).child(text))
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
            Item::Permission {
                req_id,
                title,
                plan,
                resolved,
                ..
            } => {
                let choice = |id: &'static str, label: &'static str, choice: PermChoice| {
                    let req_id = req_id.clone();
                    Button::new((id, ix))
                        .small()
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.request(Request::Resolve {
                                thread,
                                req_id: req_id.clone(),
                                choice,
                            })
                        }))
                };
                let actions = match resolved {
                    Some(choice) => div()
                        .text_sm()
                        .text_color(muted)
                        .child(resolved_label(choice, plan))
                        .into_any_element(),
                    None if plan => h_flex()
                        .gap_2()
                        .child(choice("approve", "Approve plan", PermChoice::AllowOnce).primary())
                        .child(choice("revise", "Keep planning", PermChoice::Deny))
                        .into_any_element(),
                    None => h_flex()
                        .gap_2()
                        .child(choice("once", "Allow once", PermChoice::AllowOnce).primary())
                        .child(choice("always", "Always allow", PermChoice::AllowAlways))
                        .child(choice("deny", "Deny", PermChoice::Deny).danger())
                        .into_any_element(),
                };
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(if resolved.is_none() { danger } else { border })
                    .rounded_md()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .when(!text.is_empty(), |el| {
                        el.child(TextView::markdown(("perm", ix), text))
                    })
                    .child(actions)
                    .into_any_element()
            }
            Item::Question {
                req_id,
                questions,
                answers,
            } => {
                let draft = self
                    .drafts
                    .get(&req_id)
                    .cloned()
                    .unwrap_or_else(|| vec![Vec::new(); questions.len()]);
                let mut card = v_flex()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(border)
                    .rounded_md();
                for (qi, question) in questions.iter().enumerate() {
                    let mut options = h_flex().gap_1().flex_wrap();
                    for (oi, option) in question.options.iter().enumerate() {
                        let picked = answers
                            .as_ref()
                            .unwrap_or(&draft)
                            .get(qi)
                            .is_some_and(|a| a.contains(option));
                        let (req_id, option, multi, count) = (
                            req_id.clone(),
                            option.clone(),
                            question.multi,
                            questions.len(),
                        );
                        options = options.child(
                            Button::new(("option", ix * 1000 + qi * 50 + oi))
                                .small()
                                .label(option.clone())
                                .selected(picked)
                                .disabled(answers.is_some())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let draft = this
                                        .drafts
                                        .entry(req_id.clone())
                                        .or_insert_with(|| vec![Vec::new(); count]);
                                    let slot = &mut draft[qi];
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
                    card = card
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(question.header.clone()),
                        )
                        .child(div().text_sm().child(question.text.clone()))
                        .child(options);
                }
                if answers.is_none() {
                    let req_id = req_id.clone();
                    card = card.child(
                        Button::new(("answer", ix))
                            .primary()
                            .small()
                            .label("Submit answers")
                            .on_click(cx.listener(move |this, _, _, _| {
                                let answers = this.drafts.remove(&req_id).unwrap_or_default();
                                this.request(Request::Answer {
                                    thread,
                                    req_id: req_id.clone(),
                                    answers,
                                });
                            })),
                    );
                }
                card.into_any_element()
            }
            Item::Todo { items } => v_flex()
                .gap_1()
                .p_2()
                .border_1()
                .border_color(border)
                .rounded_md()
                .children(items.into_iter().map(|todo| {
                    let mark = match todo.status {
                        TodoStatus::Pending => "[ ]",
                        TodoStatus::InProgress => "[>]",
                        TodoStatus::Done => "[x]",
                    };
                    div()
                        .text_sm()
                        .when(todo.status == TodoStatus::Done, |el| el.text_color(muted))
                        .child(format!("{mark} {}", todo.text))
                }))
                .into_any_element(),
            Item::Error { message } => div()
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
                    "Turn {turn} · {} · {input} in / {output} out",
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
                            .label("Undo this turn")
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
        };
        div().w_full().px_4().py_2().child(body).into_any_element()
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
            .label("Close")
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
                                        .label("Refresh")
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
        let (title, provider, queued) = self
            .info
            .as_ref()
            .map(|info| (info.title.clone(), info.provider.label(), info.queued))
            .unwrap_or_default();
        let pane = self.render_pane(cx);
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);

        let header = h_flex()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().text_xs().text_color(muted).child(provider))
            .when(running, |el| {
                el.child(div().text_xs().text_color(muted).child("Working…"))
            })
            .when(queued > 0, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{queued} queued")),
                )
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
            .when(running, |el| {
                el.child(
                    Button::new("stop")
                        .danger()
                        .xsmall()
                        .label("Stop")
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.request(Request::Interrupt { thread })
                        })),
                )
            })
            .child(
                Button::new("files")
                    .ghost()
                    .xsmall()
                    .label("Files")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::ListFiles { thread })
                    })),
            );

        let mode_button = |id: &'static str, label: &'static str, mode: Mode, current: Mode| {
            Button::new(id)
                .ghost()
                .xsmall()
                .label(label)
                .selected(mode == current)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mode = mode;
                    cx.notify();
                }))
        };
        let delivery_button =
            |id: &'static str, label: &'static str, delivery: Delivery, current: Delivery| {
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .label(label)
                    .selected(delivery == current)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.delivery = delivery;
                        cx.notify();
                    }))
            };
        let controls = h_flex()
            .gap_1()
            .child(mode_button("agent", "Agent", Mode::Agent, self.mode))
            .child(mode_button("plan", "Plan", Mode::Plan, self.mode))
            .child(mode_button("ask", "Ask", Mode::Ask, self.mode))
            .when(running, |el| {
                el.child(div().w(px(12.)))
                    .child(delivery_button(
                        "queue",
                        "Queue",
                        Delivery::Queue,
                        self.delivery,
                    ))
                    .child(delivery_button(
                        "steer",
                        "Steer now",
                        Delivery::SteerNow,
                        self.delivery,
                    ))
            })
            .child(div().flex_1())
            .child(
                Button::new("send")
                    .primary()
                    .small()
                    .label("Send")
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            );

        let transcript = MessageScroller::new(
            "transcript",
            self.scroller.clone(),
            cx.processor(Self::render_row),
        )
        .flex_1()
        .min_h_0();

        h_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(header)
                    .child(transcript)
                    .child(
                        v_flex()
                            .gap_1()
                            .p_3()
                            .border_t_1()
                            .border_color(border)
                            .child(Textarea::new(&self.composer))
                            .child(controls),
                    ),
            )
            .children(pane)
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
        (PermChoice::Deny, true) => "Sent back for more planning",
        (_, true) => "Plan approved",
        (PermChoice::AllowOnce, false) => "Allowed once",
        (PermChoice::AllowAlways, false) => "Always allowed",
        (PermChoice::Deny, false) => "Denied",
    }
}

fn reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "done",
        StopReason::MaxTokens => "hit the output limit",
        StopReason::Interrupted => "stopped",
        StopReason::Error => "failed",
        StopReason::Timeout => "timed out",
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

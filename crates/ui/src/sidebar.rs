//! The sidebar: the Chat/Code switch, then sessions (Code) or chats (Chat),
//! with search, a project filter, pins, a right-click menu and the footer.
//! Laid out after the mockup's `Sidebar` and `ChatSidebar` artboards.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::{ContextMenuExt as _, PopupMenuItem},
    popover::Popover,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{ProjectId, Request, ThreadId, ThreadInfo};

use crate::style::{self, SIDEBAR, provider_dot};
use crate::{Section, View, Workspace};

impl Workspace {
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (fg, muted, sidebar, primary) = (
            theme.foreground,
            theme.muted_foreground,
            theme.sidebar,
            theme.primary,
        );
        let mono = theme.mono_font_family.clone();
        let chat = self.chat;
        let query = self.editors.search.read(cx).value().trim().to_lowercase();

        let switch = h_flex()
            .p(px(3.))
            .rounded_lg()
            .bg(black().opacity(0.28))
            .border_1()
            .border_color(fg.opacity(0.07))
            .child(
                side_button("side-chat", "Chat", chat, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.switch_side(true, cx))),
            )
            .child(
                side_button("side-code", "Code", !chat, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.switch_side(false, cx))),
            );

        let head = if chat {
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .id("new-chat")
                        .gap_2p5()
                        .h(px(34.))
                        .px_2p5()
                        .rounded_lg()
                        .cursor_pointer()
                        .bg(primary.opacity(0.16))
                        .text_color(primary.opacity(0.95))
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(IconName::Plus).small())
                        .child(div().flex_1().child("New chat"))
                        .child(style::kbd("N", mono.clone(), primary.opacity(0.8)).border_0())
                        .on_click(cx.listener(|this, _, _, cx| this.new_thread(None, cx))),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_0p5()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.render_project_filter(cx)),
                )
                .child(
                    Button::new("new-thread")
                        .ghost()
                        .xsmall()
                        .icon(Lucide::SquarePen)
                        .tooltip("New thread")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let project = this.project_filter;
                            this.new_thread(project, cx)
                        })),
                )
                .child(
                    Button::new("add-project")
                        .ghost()
                        .xsmall()
                        .icon(Lucide::FolderPlus)
                        .tooltip("Add project")
                        .on_click(cx.listener(|this, _, _, cx| this.open_palette(cx))),
                )
                .into_any_element()
        };

        let search = Input::new(&self.editors.search)
            .small()
            .prefix(Icon::new(IconName::Search).small().text_color(muted))
            .suffix(style::kbd("K", mono, muted.opacity(0.8)));

        let mut visible: Vec<&ThreadInfo> = self
            .threads
            .iter()
            .filter(|t| t.chat == chat && !t.archived)
            .filter(|t| chat || self.project_filter.is_none_or(|p| t.project == Some(p)))
            .filter(|t| query.is_empty() || t.title.to_lowercase().contains(&query))
            .collect();
        visible.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
        let stamp = now();
        let (pinned, rest): (Vec<_>, Vec<_>) = visible.into_iter().partition(|t| t.pinned);
        let (today, rest): (Vec<_>, Vec<_>) = rest
            .into_iter()
            .partition(|t| stamp - t.updated_at < 86_400);
        let (week, older): (Vec<_>, Vec<_>) = rest
            .into_iter()
            .partition(|t| stamp - t.updated_at < 7 * 86_400);

        let label = |text: &'static str| {
            div()
                .px_2p5()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(text)
        };
        let mut list = v_flex()
            .id("sidebar-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_0p5()
            .px_2();
        let groups: [(&'static str, &Vec<&ThreadInfo>); 4] = [
            ("PINNED", &pinned),
            ("TODAY", &today),
            ("PREVIOUS 7 DAYS", &week),
            ("OLDER", &older),
        ];
        let mut any = false;
        for (name, threads) in groups {
            if threads.is_empty() {
                continue;
            }
            any = true;
            list = list.child(label(name));
            for thread in threads {
                list = list.child(if chat {
                    self.render_chat_row(thread, cx)
                } else {
                    self.render_card(thread, stamp, cx)
                });
            }
        }
        if !any {
            list = list.child(div().px_2p5().pt_6().text_sm().text_color(muted).child(
                if !query.is_empty() {
                    "Nothing matches."
                } else if chat {
                    "No chats yet."
                } else {
                    "No sessions yet."
                },
            ));
        }

        let archived = self
            .threads
            .iter()
            .filter(|t| t.archived && t.chat == chat)
            .count();
        let footer = v_flex()
            .gap_1p5()
            .px_2p5()
            .pt_2()
            .pb_2p5()
            .when(archived > 0, |el| {
                el.child(
                    h_flex()
                        .id("archived")
                        .gap_2()
                        .h(px(30.))
                        .px_2p5()
                        .rounded_lg()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(muted)
                        .hover(move |s| s.bg(fg.opacity(0.05)))
                        .child(Icon::new(Lucide::Archive).small())
                        .child(div().flex_1().child(if chat {
                            "Archived chats"
                        } else {
                            "Archived sessions"
                        }))
                        .child(archived.to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Settings(Section::Archived);
                            cx.notify();
                        })),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .h(px(36.))
                    .pl_2p5()
                    .pr_1()
                    .rounded_lg()
                    .bg(fg.opacity(0.04))
                    .border_1()
                    .border_color(fg.opacity(0.06))
                    .child(
                        div()
                            .size(px(20.))
                            .rounded_full()
                            .bg(fg)
                            .text_color(sidebar)
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("L"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .pl_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Local"),
                    )
                    .child(
                        Button::new("usage")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::ChartColumn)
                            .tooltip("Usage")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Usage;
                                this.request(Request::Usage {
                                    since: now() - this.usage_days * 86_400,
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("settings")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings)
                            .tooltip("Settings")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Settings(Section::General);
                                this.request(Request::CheckAuth);
                                cx.notify();
                            })),
                    ),
            );

        v_flex()
            .w(px(SIDEBAR))
            .h_full()
            .flex_shrink_0()
            .child(
                v_flex()
                    .gap_2()
                    .px_2p5()
                    .pt_1()
                    .pb_1()
                    .child(switch)
                    .child(head)
                    .child(search),
            )
            .child(list)
            .child(footer)
    }

    fn render_project_filter(&self, cx: &mut Context<Self>) -> AnyElement {
        let fg = cx.theme().foreground;
        let current = self.project_filter;
        let label = current
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
            .map_or("All projects".to_owned(), |p| p.name.clone());
        let options = std::iter::once((None, "All projects".to_owned()))
            .chain(self.projects.iter().map(|p| (Some(p.id), p.name.clone())));
        let mut list = v_flex().gap_0p5();
        for (ix, (id, name)) in options.enumerate() {
            let id: Option<ProjectId> = id;
            list = list.child(
                h_flex()
                    .id(("filter", ix))
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(id == current, |el| el.bg(fg.opacity(0.08)))
                    .hover(move |style| style.bg(fg.opacity(0.06)))
                    .child(Icon::new(IconName::Folder).small())
                    .child(div().flex_1().text_sm().child(name))
                    .when_some(id, |el, id| {
                        el.child(
                            Button::new(("project-settings", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Settings)
                                .tooltip("Project settings")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.filter_open = false;
                                    this.open_project(id, window, cx);
                                })),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.project_filter = id;
                        this.filter_open = false;
                        cx.notify();
                    })),
            );
        }
        Popover::new("project-filter")
            .anchor(Anchor::TopLeft)
            .open(self.filter_open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.filter_open = *open;
                cx.notify();
            }))
            .trigger(
                Button::new("filter-trigger").ghost().small().child(
                    h_flex()
                        .gap_2()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(IconName::Folder).small())
                        .child(label)
                        .child(Icon::new(IconName::ChevronDown).xsmall()),
                ),
            )
            .w(px(240.))
            .p_1()
            .child(list)
            .into_any_element()
    }

    /// A Code session: provider dot and title, then project and status.
    fn render_card(&self, thread: &ThreadInfo, now: i64, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, primary, warning, danger) = (
            theme.foreground,
            theme.muted_foreground,
            theme.primary,
            theme.warning,
            theme.danger,
        );
        let id = thread.id;
        let selected = self.view == View::Thread(id);
        let (status, color) = if thread.needs_input {
            ("Approval".to_owned(), Some(warning))
        } else if thread.running {
            ("Working".to_owned(), Some(primary))
        } else if thread.failed {
            ("Failed".to_owned(), Some(danger))
        } else {
            (ago(now - thread.updated_at), None)
        };
        let project = thread
            .project
            .and_then(|p| self.projects.iter().find(|x| x.id == p))
            .map_or("No project".to_owned(), |p| p.name.clone());
        let card = v_flex()
            .id(("thread", id as u64))
            .gap_1()
            .px_2p5()
            .py_2()
            .rounded_lg()
            .cursor_pointer()
            .when(selected, |el| {
                el.bg(fg.opacity(0.09))
                    .border_1()
                    .border_color(fg.opacity(0.08))
            })
            .when(!selected, |el| el.hover(move |s| s.bg(fg.opacity(0.05))))
            .child(
                h_flex()
                    .gap_2()
                    .child(provider_dot(thread.provider, 7.))
                    .child(self.title(thread, cx)),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .pl(px(15.))
                    .text_xs()
                    .text_color(muted)
                    .child(div().flex_1().min_w_0().truncate().child(project))
                    .when(thread.running && !thread.needs_input, |el| {
                        el.child(style::thinking_orb(("card-orb", id as u64), 10., primary))
                    })
                    .child(
                        div()
                            .px_1p5()
                            .rounded_md()
                            .when_some(color, |el, c| {
                                el.bg(c.opacity(0.14))
                                    .text_color(c)
                                    .font_weight(FontWeight::MEDIUM)
                            })
                            .child(status),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)));
        self.with_menu(card, thread, cx)
    }

    /// A chat: one line with its title, like the mockup's chat list.
    fn render_chat_row(&self, thread: &ThreadInfo, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, primary) = (theme.foreground, theme.muted_foreground, theme.primary);
        let id = thread.id;
        let selected = self.view == View::Thread(id);
        let row = h_flex()
            .id(("chat", id as u64))
            .gap_2()
            .h(px(32.))
            .px_2p5()
            .rounded_lg()
            .cursor_pointer()
            .when(selected, |el| {
                el.bg(fg.opacity(0.09))
                    .border_1()
                    .border_color(fg.opacity(0.07))
            })
            .when(!selected, |el| {
                el.text_color(muted.opacity(0.95))
                    .hover(move |s| s.bg(fg.opacity(0.05)))
            })
            .child(self.title(thread, cx))
            .when(thread.running, |el| {
                el.child(style::thinking_orb(
                    ("chat-row-orb", id as u64),
                    10.,
                    primary,
                ))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)));
        self.with_menu(row, thread, cx)
    }

    /// The title, or the rename field while renaming.
    fn title(&self, thread: &ThreadInfo, _: &mut Context<Self>) -> AnyElement {
        if self.renaming == Some(thread.id) {
            return div()
                .flex_1()
                .child(Input::new(&self.editors.rename).xsmall())
                .into_any_element();
        }
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .child(thread.title.clone())
            .into_any_element()
    }

    /// Pin, rename, project settings, archive and delete on right-click.
    fn with_menu(
        &self,
        element: Stateful<Div>,
        thread: &ThreadInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (requests, weak, pinned, id, project) = (
            self.requests.clone(),
            cx.entity().downgrade(),
            thread.pinned,
            thread.id,
            thread.project,
        );
        element
            .context_menu(move |menu, _, _| {
                let (r1, r2, r3) = (requests.clone(), requests.clone(), requests.clone());
                let (w1, w2) = (weak.clone(), weak.clone());
                menu.item(
                    PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" }).on_click(
                        move |_, _, _| {
                            let _ = r1.try_send(Request::PinThread { id, on: !pinned });
                        },
                    ),
                )
                .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    let _ = w1.update(cx, |this, cx| this.start_rename(id, window, cx));
                }))
                .when_some(project, |menu, project| {
                    menu.item(PopupMenuItem::new("Project settings").on_click(
                        move |_, window, cx| {
                            let _ =
                                w2.update(cx, |this, cx| this.open_project(project, window, cx));
                        },
                    ))
                })
                .separator()
                .item(PopupMenuItem::new("Archive").on_click(move |_, _, _| {
                    let _ = r2.try_send(Request::ArchiveThread { id, on: true });
                }))
                .item(PopupMenuItem::new("Delete").on_click(move |_, _, _| {
                    let _ = r3.try_send(Request::DeleteThread { id });
                }))
            })
            .into_any_element()
    }

    fn start_rename(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        self.editors
            .rename
            .update(cx, |e, cx| e.set_value(title, window, cx));
        self.renaming = Some(id);
        cx.notify();
    }
}

/// One half of the Chat/Code switch.
fn side_button(id: &'static str, label: &'static str, on: bool, cx: &App) -> Button {
    let theme = cx.theme();
    Button::new(id)
        .ghost()
        .small()
        .flex_1()
        .label(label)
        .selected(on)
        .when(on, |b| {
            b.bg(theme.foreground.opacity(0.12))
                .text_color(theme.foreground)
        })
        .when(!on, |b| b.text_color(theme.muted_foreground))
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "now", "5m", "3h", "2d".
pub(crate) fn ago(seconds: i64) -> String {
    match seconds.max(0) {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

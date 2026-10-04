//! The sidebar: the Chat/Code switch, then sessions (Code) or chats (Chat),
//! with search, a project filter, pins, a right-click menu and the footer.

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
        let (fg, muted, sidebar) = (theme.foreground, theme.muted_foreground, theme.sidebar);
        let see_through = self.prefs().glass || !self.prefs().wallpaper.is_empty();
        let chat = self.chat;
        let query = self.editors.search.read(cx).value().trim().to_lowercase();

        let side = h_flex()
            .p_0p5()
            .rounded_lg()
            .bg(fg.opacity(0.06))
            .border_1()
            .border_color(fg.opacity(0.07))
            .child(
                Button::new("side-chat")
                    .ghost()
                    .small()
                    .flex_1()
                    .label("Chat")
                    .selected(chat)
                    .on_click(cx.listener(|this, _, _, cx| this.switch_side(true, cx))),
            )
            .child(
                Button::new("side-code")
                    .ghost()
                    .small()
                    .flex_1()
                    .label("Code")
                    .selected(!chat)
                    .on_click(cx.listener(|this, _, _, cx| this.switch_side(false, cx))),
            );

        let new_button = Button::new("new-thread")
            .primary()
            .small()
            .w_full()
            .icon(IconName::Plus)
            .label(if chat { "New chat" } else { "New thread" })
            .on_click(cx.listener(|this, _, _, cx| this.new_thread(None, cx)));

        let filter = (!chat).then(|| self.render_project_filter(cx));

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

        let mut list = v_flex()
            .id("sidebar-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_0p5()
            .px_2();
        let label = |text: &'static str| {
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(text)
        };
        if !pinned.is_empty() {
            list = list.child(label("PINNED"));
            for thread in &pinned {
                list = list.child(self.render_card(thread, stamp, cx));
            }
        }
        let (today, earlier): (Vec<_>, Vec<_>) = rest
            .into_iter()
            .partition(|t| stamp - t.updated_at < 86_400);
        if !today.is_empty() {
            list = list.child(label("TODAY"));
            for thread in &today {
                list = list.child(self.render_card(thread, stamp, cx));
            }
        }
        if !earlier.is_empty() {
            list = list.child(label("EARLIER"));
            for thread in &earlier {
                list = list.child(self.render_card(thread, stamp, cx));
            }
        }
        if pinned.is_empty() && today.is_empty() && earlier.is_empty() {
            list = list.child(div().px_2().pt_6().text_sm().text_color(muted).child(
                if query.is_empty() {
                    if chat {
                        "No chats yet."
                    } else {
                        "No sessions yet."
                    }
                } else {
                    "Nothing matches."
                },
            ));
        }

        let archived = self
            .threads
            .iter()
            .filter(|t| t.archived && t.chat == chat)
            .count();
        let footer = v_flex()
            .gap_1()
            .p_2()
            .when(archived > 0, |el| {
                el.child(
                    Button::new("archived")
                        .ghost()
                        .small()
                        .w_full()
                        .justify_start()
                        .icon(Lucide::Archive)
                        .label(format!("Archived ({archived})"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Settings(Section::Archived);
                            cx.notify();
                        })),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .pl_2p5()
                    .pr_1()
                    .py_1()
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
            .bg(if see_through {
                sidebar.opacity(0.55)
            } else {
                sidebar
            })
            .border_r_1()
            .border_color(fg.opacity(0.07))
            .child(
                v_flex()
                    .gap_2()
                    .px_3()
                    .pt_1()
                    .pb_2()
                    .child(side)
                    .child(new_button),
            )
            .child(
                h_flex()
                    .gap_1()
                    .px_3()
                    .pb_1()
                    .children(filter)
                    .child(div().flex_1())
                    .when(!chat, |el| {
                        el.child(
                            Button::new("add-project")
                                .ghost()
                                .xsmall()
                                .icon(Lucide::FolderPlus)
                                .tooltip("Add project")
                                .on_click(cx.listener(|this, _, _, cx| this.open_palette(cx))),
                        )
                    }),
            )
            .child(
                div().px_3().pb_1().child(
                    Input::new(&self.editors.search)
                        .small()
                        .prefix(Icon::new(IconName::Search).small()),
                ),
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
                    .child(Icon::new(Lucide::Folder).small())
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
                Button::new("filter-trigger")
                    .ghost()
                    .small()
                    .icon(Lucide::Folder)
                    .label(label),
            )
            .w(px(240.))
            .p_1()
            .child(list)
            .into_any_element()
    }

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
            .map(|p| p.name.clone());
        let renaming = self.renaming == Some(id);
        let title: AnyElement = if renaming {
            Input::new(&self.editors.rename).xsmall().into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(thread.title.clone())
                .into_any_element()
        };

        let (requests, weak, pinned, chat) = (
            self.requests.clone(),
            cx.entity().downgrade(),
            thread.pinned,
            thread.chat,
        );
        let has_project = thread.project;
        v_flex()
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
                    .child(title),
            )
            .when(!chat || color.is_some(), |el| {
                el.child(
                    h_flex()
                        .gap_1p5()
                        .pl(px(15.))
                        .text_xs()
                        .text_color(muted)
                        .child(div().flex_1().truncate().child(project.unwrap_or_else(|| {
                            if chat {
                                String::new()
                            } else {
                                "No project".into()
                            }
                        })))
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
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)))
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
                .when_some(has_project, |menu, project| {
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

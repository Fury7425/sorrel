//! Settings pages, the project page and the usage page, laid out after the
//! mockup's Settings artboards: a nav with Back on top and grouped sections,
//! pages with a large title, and rows of title, description and control.

use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState, Textarea},
    popover::Popover,
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    Access, AuthState, Effect, OpenIn, Preferences, ProjectId, Provider, ProviderConfig, Request,
    Theme, TurnSettings,
};

use crate::sidebar::{ago, now};
use crate::style::{
    ACCENTS, SIDEBAR, card, heading, parse_hex, provider_icon, provider_tile, row, segment,
    segmented,
};
use crate::{Section, View, Workspace};

/// What a dropdown option does when picked.
type Pick = Rc<dyn Fn(&mut Workspace, &mut Context<Workspace>)>;

/// One dropdown option: an optional provider mark, its label, whether it is
/// the current value, and what picking it does.
struct Choice {
    provider: Option<Provider>,
    label: SharedString,
    on: bool,
    pick: Pick,
}

fn choice(label: impl Into<SharedString>, on: bool, pick: Pick) -> Choice {
    Choice {
        provider: None,
        label: label.into(),
        on,
        pick,
    }
}

impl Section {
    /// The nav's groups, in order.
    const GROUPS: [&'static [Section]; 3] = [
        &[Section::General, Section::Appearance],
        &[Section::Providers, Section::Connectors, Section::Tasks],
        &[Section::Archived, Section::About],
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Providers => "Providers",
            Section::Connectors => "Connectors",
            Section::Tasks => "Scheduled tasks",
            Section::Archived => "Archived sessions",
            Section::About => "About",
        }
    }

    fn icon(self) -> Icon {
        Icon::new(match self {
            Section::General => Lucide::SlidersHorizontal,
            Section::Appearance => Lucide::Palette,
            Section::Providers => Lucide::LayoutGrid,
            Section::Connectors => Lucide::Plug,
            Section::Tasks => Lucide::Clock,
            Section::Archived => Lucide::Archive,
            Section::About => Lucide::Info,
        })
    }
}

/// A scrollable page: a large title (with an optional control on its
/// right) over its sections.
fn page(
    id: &'static str,
    title: impl Into<SharedString>,
    right: Option<AnyElement>,
    children: impl IntoIterator<Item = AnyElement>,
) -> AnyElement {
    v_flex()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .items_center()
        .child(
            v_flex()
                .w_full()
                .max_w(px(820.))
                .px_6()
                .pt_6()
                .pb_12()
                .gap_2()
                .child(
                    h_flex()
                        .px_1()
                        .pb_2()
                        .child(
                            div()
                                .flex_1()
                                .text_2xl()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title.into()),
                        )
                        .children(right),
                )
                .children(children),
        )
        .into_any_element()
}

impl Workspace {
    pub(crate) fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (fg, muted, sidebar) = (theme.foreground, theme.muted_foreground, theme.sidebar);
        let see_through = self.prefs().glass || !self.prefs().wallpaper.is_empty();
        let current = match self.view {
            View::Settings(section) => Some(section),
            _ => None,
        };
        let item = |section: Section, ix: usize, cx: &mut Context<Self>| {
            let on = current == Some(section);
            h_flex()
                .id(("section", ix))
                .gap_2p5()
                .h(px(32.))
                .px_2p5()
                .rounded_lg()
                .cursor_pointer()
                .text_sm()
                .when(on, |el| {
                    el.bg(fg.opacity(0.09))
                        .border_1()
                        .border_color(fg.opacity(0.07))
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .when(!on, |el| {
                    el.text_color(muted).hover(move |s| s.bg(fg.opacity(0.05)))
                })
                .child(section.icon().small())
                .child(section.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.view = View::Settings(section);
                    cx.notify();
                }))
        };
        let mut nav = v_flex().flex_1().gap_0p5().px_2p5();
        let mut ix = 0;
        for (g, group) in Section::GROUPS.iter().enumerate() {
            if g > 0 {
                nav = nav.child(div().h(px(14.)));
            }
            for &section in group.iter() {
                nav = nav.child(item(section, ix, cx));
                ix += 1;
            }
        }
        v_flex()
            .w(px(SIDEBAR))
            .h_full()
            .flex_shrink_0()
            .bg(if see_through {
                sidebar.opacity(0.58)
            } else {
                sidebar
            })
            .border_r_1()
            .border_color(fg.opacity(0.07))
            .child(
                div().px_2p5().pt_1().pb_2p5().child(
                    h_flex()
                        .id("settings-back")
                        .gap_2p5()
                        .h(px(32.))
                        .px_2p5()
                        .rounded_lg()
                        .cursor_pointer()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .hover(move |s| s.bg(fg.opacity(0.05)))
                        .child(Icon::new(IconName::ArrowLeft).small())
                        .child("Back")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Home;
                            cx.notify();
                        })),
                ),
            )
            .child(nav)
            .child(
                div().p_2p5().child(
                    h_flex()
                        .gap_2()
                        .h(px(36.))
                        .px_2p5()
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
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                        ),
                ),
            )
    }

    pub(crate) fn render_settings(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        match section {
            Section::General => self.render_general(cx),
            Section::Appearance => self.render_appearance(cx),
            Section::Providers => self.render_providers(cx),
            Section::Connectors => self.render_connectors(cx),
            Section::Tasks => self.render_tasks(cx),
            Section::Archived => self.render_archived(cx),
            Section::About => self.render_about(cx),
        }
    }

    /// A select: a bordered button showing the value, opening a list.
    fn dropdown(
        &self,
        id: &'static str,
        value: AnyElement,
        width: f32,
        choices: Vec<Choice>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, popover) = (theme.foreground, theme.muted_foreground, theme.popover);
        let list = v_flex()
            .gap_0p5()
            .children(choices.into_iter().enumerate().map(|(ix, c)| {
                let pick = c.pick.clone();
                h_flex()
                    .id((id, ix))
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .when(c.on, |el| el.bg(fg.opacity(0.08)))
                    .hover(move |s| s.bg(fg.opacity(0.06)))
                    .children(c.provider.map(|p| provider_icon(p, 14.)))
                    .child(div().flex_1().child(c.label))
                    .when(c.on, |el| el.child(Icon::new(IconName::Check).small()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_select = None;
                        pick(this, cx);
                        cx.notify();
                    }))
            }));
        Popover::new(id)
            .anchor(Anchor::TopRight)
            .open(self.open_select == Some(id))
            .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                this.open_select = open.then_some(id);
                cx.notify();
            }))
            .trigger(
                Button::new(id).outline().small().child(
                    h_flex()
                        .gap_2()
                        .min_w(px(width - 26.))
                        .child(div().flex_1().child(value))
                        .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
                ),
            )
            .w(px(width.max(200.)))
            .p_1()
            .bg(popover.opacity(0.96))
            .child(list)
            .into_any_element()
    }

    fn render_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let prefs = self.prefs().clone();
        let max = self.settings.max_sessions;
        let open_in = segmented(cx).children(
            [
                (OpenIn::Chat, "Chat"),
                (OpenIn::Code, "Code"),
                (OpenIn::Last, "Last used"),
            ]
            .map(|(value, label)| {
                segment(
                    cx,
                    ("open-in", value as usize),
                    label,
                    prefs.open_in == value,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.set_prefs(|p| p.open_in = value, cx)),
                )
            }),
        );

        // Model: every enabled CLI's default and models, as one list.
        let enabled: Vec<Provider> = self
            .settings
            .providers
            .iter()
            .filter(|(_, c)| !c.disabled)
            .map(|(p, _)| *p)
            .collect();
        let mut models = Vec::new();
        for &p in &enabled {
            let mut entries = vec![(None, "Default".to_owned())];
            entries.extend(
                self.catalog
                    .get(&p)
                    .into_iter()
                    .flatten()
                    .map(|m| (Some(m.id.clone()), m.label.clone())),
            );
            for (id, label) in entries {
                let on = prefs.provider == p && prefs.settings.model == id;
                let pick: Pick = Rc::new(move |this: &mut Workspace, cx| {
                    let id = id.clone();
                    this.set_prefs(
                        |prefs| {
                            prefs.provider = p;
                            prefs.settings = TurnSettings {
                                model: id,
                                effort: None,
                                fast: false,
                                long_context: false,
                                ..prefs.settings.clone()
                            };
                        },
                        cx,
                    )
                });
                models.push(Choice {
                    provider: Some(p),
                    label: format!("{} · {label}", short_label(p)).into(),
                    on,
                    pick,
                });
            }
        }
        let current_model = prefs
            .settings
            .model
            .as_ref()
            .and_then(|id| {
                self.catalog
                    .get(&prefs.provider)?
                    .iter()
                    .find(|m| m.id == *id)
                    .map(|m| m.label.clone())
            })
            .unwrap_or_else(|| "Default".into());
        let model_value = h_flex()
            .gap_2()
            .child(provider_icon(prefs.provider, 14.))
            .child(format!("{} {current_model}", short_label(prefs.provider)))
            .into_any_element();
        let model = self.dropdown("default-model", model_value, 210., models, cx);

        let access = self.dropdown(
            "default-access",
            h_flex()
                .gap_2()
                .child(Icon::new(Lucide::Lock).small())
                .child(prefs.settings.access.label())
                .into_any_element(),
            210.,
            Access::ALL
                .into_iter()
                .map(|a| {
                    choice(
                        a.label(),
                        prefs.settings.access == a,
                        Rc::new(move |this: &mut Workspace, cx| {
                            this.set_prefs(|p| p.settings.access = a, cx)
                        }),
                    )
                })
                .collect(),
            cx,
        );
        let stepper = h_flex()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().foreground.opacity(0.1))
            .child(
                Button::new("sessions-down")
                    .ghost()
                    .small()
                    .icon(IconName::Minus)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::SetMaxSessions {
                            count: max.saturating_sub(1),
                        })
                    })),
            )
            .child(
                div()
                    .w(px(30.))
                    .text_center()
                    .text_sm()
                    .child(max.to_string()),
            )
            .child(
                Button::new("sessions-up")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::SetMaxSessions { count: max + 1 })
                    })),
            );
        let follow = self.dropdown(
            "follow-up",
            div()
                .child(if prefs.enter_steers { "Steer" } else { "Queue" })
                .into_any_element(),
            120.,
            vec![
                choice(
                    "Queue",
                    !prefs.enter_steers,
                    Rc::new(|this: &mut Workspace, cx| {
                        this.set_prefs(|p| p.enter_steers = false, cx)
                    }),
                ),
                choice(
                    "Steer",
                    prefs.enter_steers,
                    Rc::new(|this: &mut Workspace, cx| {
                        this.set_prefs(|p| p.enter_steers = true, cx)
                    }),
                ),
            ],
            cx,
        );
        let updates = Switch::new("check-updates")
            .checked(prefs.check_updates)
            .on_click(cx.listener(|this, on: &bool, _, cx| {
                let on = *on;
                this.set_prefs(|p| p.check_updates = on, cx)
            }));
        let restore = Button::new("restore-defaults")
            .ghost()
            .small()
            .icon(IconName::Undo)
            .label("Restore defaults")
            .on_click(
                cx.listener(|this, _, _, cx| this.set_prefs(|p| *p = Preferences::default(), cx)),
            );
        let muted = cx.theme().muted_foreground;

        page(
            "general",
            "General",
            Some(restore.into_any_element()),
            [
                heading("New threads", cx).into_any_element(),
                card(cx)
                    .child(row(
                        "Open in",
                        "Chat is a plain conversation. Code works in a project folder with tools, files and approvals. Switch any time at the top of the sidebar.",
                        open_in,
                        false,
                        cx,
                    ))
                    .child(row(
                        "Model",
                        "The model new threads start on. The chip in a thread can change it.",
                        model,
                        false,
                        cx,
                    ))
                    .child(row(
                        "Permissions",
                        "How much a new thread may do before it asks.",
                        access,
                        false,
                        cx,
                    ))
                    .child(row(
                        "Sessions at once",
                        "Turns beyond this wait for a free slot.",
                        stepper,
                        true,
                        cx,
                    ))
                    .into_any_element(),
                heading("Behavior", cx).into_any_element(),
                card(cx)
                    .child(row(
                        "Follow-up behavior",
                        "What Enter does while the agent runs. Ctrl+Enter does the other.",
                        follow,
                        false,
                        cx,
                    ))
                    .child(row(
                        "Check for updates",
                        "Look for a new Sorrel release on start.",
                        updates,
                        true,
                        cx,
                    ))
                    .into_any_element(),
                heading("Memory", cx).into_any_element(),
                card(cx)
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Shared with every session through CLAUDE.md and AGENTS.md."),
                    )
                    .child(Textarea::new(&self.editors.memory))
                    .child(
                        h_flex().child(
                            Button::new("save-memory")
                                .outline()
                                .small()
                                .label("Save memory")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let text = this.editors.memory.read(cx).value().to_string();
                                    this.request(Request::SetMemory { text });
                                })),
                        ),
                    )
                    .into_any_element(),
            ],
        )
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let prefs = self.prefs().clone();
        let theme = cx.theme();
        let (fg, muted, primary) = (theme.foreground, theme.muted_foreground, theme.primary);

        // Three preview cards: a small window drawn in each scheme.
        let light = (
            rgb(0xf3f3f6).into(),
            rgb(0xe2e2e8).into(),
            rgb(0xffffff).into(),
        );
        let dark = (
            rgb(0x141418).into(),
            rgb(0x1d1d23).into(),
            rgb(0x0c0c10).into(),
        );
        let preview = |(ground, side, pane): (Hsla, Hsla, Hsla)| {
            h_flex()
                .flex_1()
                .h_full()
                .gap_1p5()
                .p_2p5()
                .bg(ground)
                .child(div().w(relative(0.3)).h_full().rounded_md().bg(side))
                .child(div().flex_1().h_full().rounded_md().bg(pane))
        };
        let schemes = h_flex().gap_3().children(
            [
                (Theme::System, "System", Lucide::Monitor),
                (Theme::Light, "Light", Lucide::Sun),
                (Theme::Dark, "Dark", Lucide::Moon),
            ]
            .map(|(value, label, icon)| {
                let on = prefs.theme == value;
                let body = match value {
                    Theme::System => h_flex()
                        .size_full()
                        .child(preview(light))
                        .child(preview(dark)),
                    Theme::Light => h_flex().size_full().child(preview(light)),
                    Theme::Dark => h_flex().size_full().child(preview(dark)),
                };
                v_flex()
                    .id(("scheme", value as usize))
                    .flex_1()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .w_full()
                            .h(px(96.))
                            .rounded_xl()
                            .overflow_hidden()
                            .border_2()
                            .border_color(if on { primary } else { fg.opacity(0.1) })
                            .child(body),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if on { primary } else { muted })
                            .child(Icon::new(icon).small())
                            .child(label),
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_prefs(|p| p.theme = value, cx)),
                    )
            }),
        );

        let swatches = h_flex()
            .gap_2()
            .children(ACCENTS.iter().enumerate().map(|(ix, hex)| {
                let on = prefs.accent == *hex;
                let swatch = div()
                    .id(("accent", ix))
                    .size(px(26.))
                    .rounded_lg()
                    .cursor_pointer()
                    .when(on, |el| el.border_2().border_color(fg));
                let swatch = match parse_hex(hex) {
                    Some(color) => swatch.bg(color),
                    // The theme's own accent, shown as a blend.
                    None => swatch.bg(linear_gradient(
                        135.,
                        linear_color_stop(rgb(0x8b7cf6), 0.),
                        linear_color_stop(rgb(0x5b9cf5), 1.),
                    )),
                };
                swatch.on_click(cx.listener(move |this, _, _, cx| {
                    this.set_prefs(|p| p.accent = hex.to_string(), cx)
                }))
            }));

        let glass = segmented(cx)
            .child(
                segment(cx, "glass-frosted", "Frosted", prefs.glass)
                    .on_click(cx.listener(|this, _, _, cx| this.set_prefs(|p| p.glass = true, cx))),
            )
            .child(
                segment(cx, "glass-opaque", "Opaque", !prefs.glass).on_click(
                    cx.listener(|this, _, _, cx| this.set_prefs(|p| p.glass = false, cx)),
                ),
            );

        let has_wallpaper = !prefs.wallpaper.is_empty();
        let name = std::path::Path::new(&prefs.wallpaper)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "None".into());
        let wallpaper = h_flex()
            .gap_3p5()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(fg.opacity(0.05))
            .child(
                div()
                    .size(px(44.))
                    .flex_shrink_0()
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(fg.opacity(0.06))
                    .when(has_wallpaper, |el| {
                        el.child(
                            img(std::path::PathBuf::from(&prefs.wallpaper))
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("New thread background"),
                    )
                    .child(div().text_xs().text_color(muted).truncate().child(name)),
            )
            .child(
                Button::new("choose-wallpaper")
                    .outline()
                    .small()
                    .label(if has_wallpaper {
                        "Replace image"
                    } else {
                        "Choose image"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.pick_wallpaper(cx))),
            )
            .when(has_wallpaper, |el| {
                el.child(
                    Button::new("remove-wallpaper")
                        .danger()
                        .small()
                        .outline()
                        .label("Remove")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_prefs(|p| p.wallpaper.clear(), cx)
                        })),
                )
            });
        let effect = self.dropdown(
            "background-effect",
            div().child(prefs.effect.label()).into_any_element(),
            140.,
            Effect::ALL
                .into_iter()
                .map(|e| {
                    choice(
                        e.label(),
                        prefs.effect == e,
                        Rc::new(move |this: &mut Workspace, cx| {
                            this.set_prefs(|p| p.effect = e, cx)
                        }),
                    )
                })
                .collect(),
            cx,
        );

        page(
            "appearance",
            "Appearance",
            None,
            [
                heading("Color scheme", cx).into_any_element(),
                schemes.into_any_element(),
                div().h(px(4.)).into_any_element(),
                card(cx)
                    .child(row(
                        "Accent color",
                        "Buttons, selection and the effort slider.",
                        swatches,
                        true,
                        cx,
                    ))
                    .into_any_element(),
                heading("Material and background", cx).into_any_element(),
                card(cx)
                    .child(row(
                        "Glass",
                        "Frosted lets the desktop show through, blurred. Opaque is solid and a little faster.",
                        glass,
                        false,
                        cx,
                    ))
                    .child(wallpaper)
                    .child(row(
                        "Background effect",
                        "A texture over the wallpaper.",
                        effect,
                        true,
                        cx,
                    ))
                    .into_any_element(),
            ],
        )
    }

    fn provider_config(&self, provider: Provider) -> ProviderConfig {
        self.settings
            .providers
            .iter()
            .find(|(p, _)| *p == provider)
            .map(|(_, c)| c.clone())
            .unwrap_or_default()
    }

    /// Saves the Providers page's binary path and arguments for `provider`.
    pub(crate) fn save_runtime(&mut self, provider: Provider, cx: &mut Context<Self>) {
        let read = |list: &Vec<(Provider, Entity<InputState>)>, cx: &App| {
            list.iter()
                .find(|(p, _)| *p == provider)
                .map(|(_, e)| e.read(cx).value().trim().to_string())
                .unwrap_or_default()
        };
        let mut config = self.provider_config(provider);
        let (binary, args) = (
            read(&self.editors.binaries, cx),
            read(&self.editors.args, cx),
        );
        if config.binary == binary && config.args == args {
            return;
        }
        config.binary = binary;
        config.args = args;
        self.request(Request::SetProviderConfig { provider, config });
    }

    /// Saves the API key typed for `provider` and clears the field.
    pub(crate) fn save_key(
        &mut self,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((_, key)) = self
            .editors
            .keys
            .iter()
            .find(|(p, _)| *p == provider)
            .cloned()
        {
            let value = key.read(cx).value().trim().to_string();
            if value.is_empty() {
                return;
            }
            key.update(cx, |e, cx| e.set_value("", window, cx));
            self.request(Request::SetApiKey {
                provider,
                key: Some(value),
            });
        }
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, danger, primary, mono) = (
            theme.foreground,
            theme.muted_foreground,
            theme.danger,
            theme.primary,
            theme.mono_font_family.clone(),
        );
        let open = self.provider_open;

        let cards: Vec<AnyElement> = Provider::ALL
            .iter()
            .map(|&p| {
                let config = self.provider_config(p);
                let status = self.auth.iter().find(|a| a.provider == p).cloned();
                let state = status.as_ref().map_or(AuthState::Unknown, |s| s.state);
                let version = status
                    .as_ref()
                    .and_then(|s| {
                        s.version
                            .split_whitespace()
                            .map(|w| w.trim_start_matches('v'))
                            .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
                            .map(|w| format!("v{w}"))
                    })
                    .unwrap_or_default();
                let state_text = match state {
                    AuthState::Unknown if status.is_none() => "Checking",
                    AuthState::Unknown => "Installed",
                    AuthState::Missing => "Not installed",
                    AuthState::SignedOut => "Signed out",
                    AuthState::Subscription => "Signed in",
                    AuthState::ApiKey => "Using an API key",
                };
                let subtitle = match (config.disabled, version.is_empty()) {
                    (true, _) => "Off".to_owned(),
                    (false, true) => state_text.to_owned(),
                    (false, false) => format!("{version} · {state_text}"),
                };
                let is_open = open == Some(p);

                let header = h_flex()
                    .id(("provider", p as usize))
                    .gap_4()
                    .px_4()
                    .py_3p5()
                    .cursor_pointer()
                    .child(provider_tile(p, 44.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when(config.disabled, |el| el.text_color(muted))
                                    .child(p.label()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(match state {
                                        AuthState::Missing if !config.disabled => danger,
                                        _ => muted,
                                    })
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        Icon::new(if is_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .text_color(muted),
                    )
                    .child(
                        Switch::new(("provider-on", p as usize))
                            .checked(!config.disabled)
                            .on_click(cx.listener(move |this, on: &bool, _, _| {
                                let mut config = this.provider_config(p);
                                config.disabled = !*on;
                                this.request(Request::SetProviderConfig {
                                    provider: p,
                                    config,
                                });
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.provider_open == Some(p) {
                            this.provider_open = None;
                        } else {
                            this.provider_open = Some(p);
                            this.env_adding = false;
                            if !this.catalog.contains_key(&p) {
                                this.request(Request::ListModels { provider: p });
                            }
                        }
                        cx.notify();
                    }));

                card(cx)
                    .overflow_hidden()
                    .child(header)
                    .when(is_open, |el| {
                        el.child(self.render_provider_body(
                            p,
                            &config,
                            status.as_ref(),
                            (fg, muted, primary, mono.clone()),
                            cx,
                        ))
                    })
                    .into_any_element()
            })
            .collect();

        let check = Button::new("providers-check")
            .ghost()
            .small()
            .icon(IconName::RefreshCw)
            .label("Detect again")
            .on_click(cx.listener(|this, _, _, _| this.request(Request::CheckAuth)));
        page(
            "providers",
            "Providers",
            Some(check.into_any_element()),
            [v_flex().gap_3().children(cards).into_any_element()],
        )
    }

    /// An open provider card: account, runtime, environment and models.
    fn render_provider_body(
        &self,
        selected: Provider,
        config: &ProviderConfig,
        status: Option<&proto::AuthStatus>,
        (fg, muted, primary, mono): (Hsla, Hsla, Hsla, SharedString),
        cx: &mut Context<Self>,
    ) -> Div {
        let state = status.map_or(AuthState::Unknown, |s| s.state);
        let detail = status.map(|s| s.detail.clone()).unwrap_or_default();
        let account = status.map(|s| s.account.clone()).unwrap_or_default();
        let found = status.map(|s| (s.bin.clone(), s.version.clone())).unwrap_or_default();
        let key_set = self.settings.api_key_set.contains(&selected);
        let using_key = self.settings.use_api_key.contains(&selected);
        let editor = |list: &Vec<(Provider, Entity<InputState>)>| {
            list.iter()
                .find(|(p, _)| *p == selected)
                .map(|(_, e)| e.clone())
        };
        let (key, binary, args) = (
            editor(&self.editors.keys),
            editor(&self.editors.binaries),
            editor(&self.editors.args),
        );
        let models = self.catalog.get(&selected).cloned().unwrap_or_default();
        let signed_in = matches!(state, AuthState::Subscription | AuthState::ApiKey);

        let section = |title: &'static str| {
            h_flex()
                .pt_5()
                .pb_1()
                .text_base()
                .text_color(muted)
                .child(div().flex_1().child(title))
        };

        // Who the CLI is signed in as, like a one-account list.
        let name = if !account.is_empty() {
            account.clone()
        } else {
            match state {
                AuthState::Subscription => format!("{} account", short_label(selected)),
                AuthState::ApiKey => "API key".to_owned(),
                AuthState::Missing => "Not installed".to_owned(),
                AuthState::SignedOut => "Not signed in".to_owned(),
                AuthState::Unknown => short_label(selected).to_owned(),
            }
        };
        let initial = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default();
        let account_row = h_flex()
            .gap_3()
            .py_3()
            .border_b_1()
            .border_color(fg.opacity(0.06))
            .child(
                div()
                    .size(px(40.))
                    .flex_shrink_0()
                    .rounded_full()
                    .border_2()
                    .border_color(if signed_in { primary } else { fg.opacity(0.15) })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(initial),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).truncate().child(name))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .text_color(muted)
                            .when(!detail.is_empty() && state != AuthState::Missing, |el| {
                                el.child(div().truncate().child(detail.clone()))
                            })
                            .when(signed_in, |el| {
                                el.child("·").child(div().text_color(primary).child("In use"))
                            }),
                    ),
            );

        let install = install_command(selected);
        let sign_in_row = if state == AuthState::Missing {
            h_flex()
                .gap_3()
                .py_2p5()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_family(mono.clone())
                        .text_color(muted)
                        .child(install),
                )
                .child(
                    Button::new("copy-install")
                        .outline()
                        .small()
                        .label("Copy install command")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(install.to_owned()));
                        })),
                )
        } else {
            h_flex().py_1().child(
                Button::new("sign-in")
                    .ghost()
                    .icon(IconName::Plus)
                    .label(if signed_in { "Sign in with another account" } else { "Sign in" })
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::SignIn { provider: selected })
                    })),
            )
        };

        let api_key = row(
            "Use an API key instead",
            if key_set {
                "A key is saved on this machine. Turn it on to use it instead of the CLI's own sign-in."
            } else {
                "Saved on this machine and handed to the CLI. Press Enter to save."
            },
            h_flex()
                .gap_2()
                .children(key.map(|key| div().w(px(200.)).child(Input::new(&key).small())))
                .child(
                    Switch::new("use-key")
                        .checked(using_key)
                        .disabled(!key_set)
                        .on_click(cx.listener(move |this, on: &bool, _, _| {
                            this.request(Request::SetUseApiKey {
                                provider: selected,
                                on: *on,
                            })
                        })),
                )
                .when(key_set, |el| {
                    el.child(
                        Button::new("remove-key")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Forget the key")
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.request(Request::SetApiKey {
                                    provider: selected,
                                    key: None,
                                })
                            })),
                    )
                }),
            true,
            cx,
        )
        .px_0();

        let binary_help = match found {
            (bin, _) if bin.is_empty() => format!(
                "{} was not detected. Leave empty to search PATH and the usual install folders, or point to it. Saved when you press Enter or leave the field.",
                short_label(selected)
            ),
            (bin, version) if version.is_empty() => format!("Detected at {bin}."),
            (bin, version) => format!("Detected at {bin} · {version}"),
        };
        let runtime = v_flex()
            .child(
                row(
                    "Binary path",
                    binary_help,
                    div().w(px(260.)).children(binary.map(|b| Input::new(&b).small())),
                    false,
                    cx,
                )
                .px_0(),
            )
            .child(
                row(
                    "Launch arguments",
                    "Extra arguments added when a session starts.",
                    div().w(px(260.)).children(args.map(|a| Input::new(&a).small())),
                    true,
                    cx,
                )
                .px_0(),
            );

        let env_rows = config.env.iter().enumerate().map(|(ix, (k, v))| {
            let key = k.clone();
            h_flex()
                .gap_3()
                .py_2p5()
                .border_b_1()
                .border_color(fg.opacity(0.05))
                .text_sm()
                .font_family(mono.clone())
                .child(div().w(px(220.)).truncate().child(k.clone()))
                .child(div().flex_1().truncate().text_color(muted).child(v.clone()))
                .child(
                    Button::new(("remove-env", ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Remove variable")
                        .on_click(cx.listener(move |this, _, _, _| {
                            let mut config = this.provider_config(selected);
                            config.env.retain(|(k, _)| *k != key);
                            this.request(Request::SetProviderConfig {
                                provider: selected,
                                config,
                            });
                        })),
                )
                .into_any_element()
        });
        let add_row = self.env_adding.then(|| {
            h_flex()
                .gap_2()
                .py_2p5()
                .border_b_1()
                .border_color(fg.opacity(0.05))
                .child(
                    div()
                        .w(px(200.))
                        .child(Input::new(&self.editors.env_key).small()),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&self.editors.env_value).small()),
                )
                .child(
                    Button::new("add-env")
                        .primary()
                        .small()
                        .label("Add")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let key = this.editors.env_key.read(cx).value().trim().to_string();
                            let value = this.editors.env_value.read(cx).value().to_string();
                            if key.is_empty() {
                                return;
                            }
                            let mut config = this.provider_config(selected);
                            config.env.retain(|(k, _)| *k != key);
                            config.env.push((key, value));
                            this.editors
                                .env_key
                                .update(cx, |e, cx| e.set_value("", window, cx));
                            this.editors
                                .env_value
                                .update(cx, |e, cx| e.set_value("", window, cx));
                            this.env_adding = false;
                            this.request(Request::SetProviderConfig {
                                provider: selected,
                                config,
                            });
                            cx.notify();
                        })),
                )
        });
        let environment = v_flex().children(env_rows).children(add_row).child(
            h_flex()
                .gap_4()
                .py_2p5()
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(muted)
                        .child("Passed to this CLI only. Never put a subscription token here."),
                )
                .child(
                    Button::new("show-add-env")
                        .outline()
                        .small()
                        .icon(IconName::Plus)
                        .label("Add variable")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.env_adding = !this.env_adding;
                            cx.notify();
                        })),
                ),
        );

        let model_chips = h_flex()
            .flex_wrap()
            .gap_2()
            .py_2()
            .children(if models.is_empty() {
                vec![
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("This CLI picks its own models.")
                        .into_any_element(),
                ]
            } else {
                models
                    .into_iter()
                    .map(|m| {
                        div()
                            .px_2p5()
                            .py_1()
                            .rounded_md()
                            .bg(fg.opacity(0.06))
                            .border_1()
                            .border_color(fg.opacity(0.07))
                            .text_sm()
                            .child(m.label)
                            .into_any_element()
                    })
                    .collect()
            });

        // Indented to line up with the name beside the tile, as in Zeron.
        v_flex()
            .pl(px(16. + 44. + 16.))
            .pr_4()
            .pb_4()
            .child(
                section("Accounts").child(
                    Button::new("check-auth")
                        .ghost()
                        .small()
                        .icon(IconName::RefreshCw)
                        .tooltip("Check again")
                        .on_click(cx.listener(|this, _, _, _| this.request(Request::CheckAuth))),
                ),
            )
            .child(account_row)
            .child(sign_in_row)
            .child(api_key)
            .child(section("Runtime"))
            .child(runtime)
            .child(section("Environment"))
            .child(environment)
            .child(section("Models"))
            .child(model_chips)
    }

    fn render_connectors(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        page(
            "connectors",
            "Connectors",
            None,
            [
                heading("MCP servers", cx).into_any_element(),
                card(cx)
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("A JSON list, passed to every CLI when a session starts."),
                    )
                    .child(Textarea::new(&self.editors.mcp))
                    .child(
                        h_flex().child(
                            Button::new("save-mcp")
                                .outline()
                                .small()
                                .label("Save connectors")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let json = this.editors.mcp.read(cx).value().to_string();
                                    this.request(Request::SetMcpServers { json });
                                })),
                        ),
                    )
                    .into_any_element(),
            ],
        )
    }

    fn render_tasks(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let stamp = now();
        let projects = segmented(cx)
            .child(
                segment(
                    cx,
                    "task-project-none",
                    "No project",
                    self.task_project.is_none(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.task_project = None;
                    cx.notify();
                })),
            )
            .children(self.projects.iter().map(|p| {
                let id = p.id;
                segment(
                    cx,
                    ("task-project", id as u64),
                    p.name.clone(),
                    self.task_project == Some(id),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.task_project = Some(id);
                    cx.notify();
                }))
            }));
        let providers = segmented(cx).children(Provider::ALL.map(|p| {
            segment(
                cx,
                ("task-provider", p as usize),
                short_label(p),
                self.task_provider == p,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.task_provider = p;
                cx.notify();
            }))
        }));
        let list = self.tasks.iter().map(|task| {
            let id = task.id;
            let schedule = match task.every_minutes {
                Some(minutes) => format!("every {minutes} min"),
                None => "once".into(),
            };
            let next = if task.next_run < 0 {
                "done".to_owned()
            } else if task.next_run <= stamp {
                "due now".to_owned()
            } else {
                format!("next in {} min", (task.next_run - stamp + 59) / 60)
            };
            let thread = task.thread;
            card(cx)
                .p_3()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .child(task.prompt.chars().take(200).collect::<String>()),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(muted)
                        .child(provider_icon(task.provider, 12.))
                        .child(format!(
                            "{} · {schedule} · {next} · {}",
                            task.provider.label(),
                            if task.last_status.is_empty() {
                                "not run yet"
                            } else {
                                task.last_status.as_str()
                            }
                        )),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("run-task", id as u64))
                                .outline()
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
                .into_any_element()
        });
        let form = card(cx)
            .p_4()
            .gap_3()
            .child(div().text_xs().text_color(muted).child(
                "Prompts that run on their own and post results to a thread. Nobody is there to approve, so anything that needs approval is denied.",
            ))
            .child(Textarea::new(&self.editors.task_prompt))
            .child(projects)
            .child(providers)
            .child(Input::new(&self.editors.task_every).small())
            .child(
                h_flex().child(
                    Button::new("create-task")
                        .primary()
                        .small()
                        .label("Create task")
                        .on_click(cx.listener(|this, _, window, cx| {
                            let prompt = this.editors.task_prompt.read(cx).value().to_string();
                            let every = this
                                .editors
                                .task_every
                                .read(cx)
                                .value()
                                .trim()
                                .parse::<u32>()
                                .ok();
                            this.request(Request::CreateTask {
                                project: this.task_project,
                                provider: this.task_provider,
                                prompt,
                                every_minutes: every,
                            });
                            this.editors
                                .task_prompt
                                .update(cx, |e, cx| e.set_value("", window, cx));
                            this.editors
                                .task_every
                                .update(cx, |e, cx| e.set_value("", window, cx));
                        })),
                ),
            );
        page(
            "tasks",
            "Scheduled tasks",
            None,
            std::iter::once(form.into_any_element()).chain(list),
        )
    }

    fn render_archived(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let stamp = now();
        let archived: Vec<_> = self.threads.iter().filter(|t| t.archived).collect();
        let body = if archived.is_empty() {
            card(cx).p_4().child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Archived sessions and chats show up here."),
            )
        } else {
            card(cx).children(archived.iter().enumerate().map(|(ix, t)| {
                let id = t.id;
                row(
                    t.title.clone(),
                    format!(
                        "{} · {} · {}",
                        if t.chat { "Chat" } else { "Code" },
                        t.provider.label(),
                        ago(stamp - t.updated_at)
                    ),
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("unarchive", id as u64))
                                .outline()
                                .xsmall()
                                .label("Restore")
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.request(Request::ArchiveThread { id, on: false })
                                })),
                        )
                        .child(
                            Button::new(("delete-archived", id as u64))
                                .ghost()
                                .xsmall()
                                .label("Delete")
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.request(Request::DeleteThread { id })
                                })),
                        ),
                    ix + 1 == archived.len(),
                    cx,
                )
            }))
        };
        page(
            "archived",
            "Archived sessions",
            None,
            [body.into_any_element()],
        )
    }

    fn render_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        page(
            "about",
            "About",
            None,
            [card(cx)
                .child(row(
                    "Version",
                    format!("Sorrel {}", env!("CARGO_PKG_VERSION")),
                    div(),
                    false,
                    cx,
                ))
                .child(row(
                    "Data folder",
                    self.settings.data_dir.display().to_string(),
                    div(),
                    false,
                    cx,
                ))
                .child(div().px_4().py_3().text_xs().text_color(muted).child(
                    "Sorrel runs the official CLIs you already use. Sign-in stays inside each CLI; Sorrel never reads their credentials.",
                ))
                .into_any_element()],
        )
    }

    pub(crate) fn render_project(&self, id: ProjectId, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(project) = self.projects.iter().find(|p| p.id == id) else {
            return page(
                "project",
                "Project",
                None,
                [div()
                    .text_sm()
                    .child("This project was removed.")
                    .into_any_element()],
            );
        };
        let folder = project.folder.clone();
        page(
            "project",
            project.name.clone(),
            None,
            [card(cx)
                .p_4()
                .gap_3()
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
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Instructions"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Written to CLAUDE.md and AGENTS.md in the folder."),
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
                                    let name =
                                        this.editors.project_name.read(cx).value().to_string();
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
                                .outline()
                                .small()
                                .icon(IconName::Plus)
                                .label("New thread")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.chat = false;
                                    this.new_thread(Some(id), cx)
                                })),
                        )
                        .child(
                            Button::new("project-folder")
                                .ghost()
                                .small()
                                .icon(IconName::FolderOpen)
                                .label("Open folder")
                                .on_click(move |_, _, cx| {
                                    cx.open_url(&crate::thread::file_url(&folder))
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
                .into_any_element()],
        )
    }

    pub(crate) fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, primary) = (theme.foreground, theme.muted_foreground, theme.primary);
        let days = self.usage_days;
        let range = segmented(cx).children([(1, "Past 24h"), (7, "7 days"), (30, "30 days")].map(
            |(n, label)| {
                segment(cx, ("usage-range", n as u64), label, days == n).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.usage_days = n;
                        this.request(Request::Usage {
                            since: now() - n * 86_400,
                        });
                        cx.notify();
                    },
                ))
            },
        ));
        let (rows, daily) = self.usage.clone().unwrap_or_default();
        let total_in: u64 = rows.iter().map(|r| r.input).sum();
        let total_out: u64 = rows.iter().map(|r| r.output).sum();
        let turns: u64 = rows.iter().map(|r| r.turns).sum();
        let tile = |label: &'static str, value: String| {
            card(cx)
                .flex_1()
                .p_4()
                .gap_1p5()
                .child(div().text_xs().text_color(muted).child(label))
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::MEDIUM)
                        .child(value),
                )
        };
        let peak = daily.iter().map(|d| d.1).max().unwrap_or(1).max(1);
        let stamp = now();
        let bars = h_flex()
            .items_end()
            .gap_3()
            .h(px(170.))
            .children(daily.iter().map(|(day, tokens)| {
                let height = 140. * *tokens as f32 / peak as f32;
                v_flex()
                    .flex_1()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .w_full()
                            .h(px(height.max(2.)))
                            .rounded_t_md()
                            .bg(primary.opacity(0.85)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(if stamp - day < 86_400 {
                                "today".to_owned()
                            } else {
                                format!("{} ago", ago(stamp - day))
                            }),
                    )
            }));
        let table = card(cx)
            .child(
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_2p5()
                    .border_b_1()
                    .border_color(fg.opacity(0.05))
                    .text_xs()
                    .text_color(muted)
                    .child(div().flex_1().child("Provider"))
                    .child(div().w(px(110.)).child("Input"))
                    .child(div().w(px(110.)).child("Output"))
                    .child(div().w(px(80.)).child("Turns"))
                    .child(div().w(px(80.)).child("Threads")),
            )
            .children(rows.iter().enumerate().map(|(ix, r)| {
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_2p5()
                    .when(ix + 1 < rows.len(), |el| {
                        el.border_b_1().border_color(fg.opacity(0.05))
                    })
                    .text_sm()
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .child(provider_icon(r.provider, 14.))
                            .child(r.provider.label()),
                    )
                    .child(div().w(px(110.)).child(short(r.input)))
                    .child(div().w(px(110.)).child(short(r.output)))
                    .child(
                        div()
                            .w(px(80.))
                            .text_color(muted)
                            .child(r.turns.to_string()),
                    )
                    .child(
                        div()
                            .w(px(80.))
                            .text_color(muted)
                            .child(r.threads.to_string()),
                    )
                    .into_any_element()
            }));
        page(
            "usage",
            "Usage",
            Some(range.into_any_element()),
            [
                h_flex()
                    .gap_3()
                    .child(tile("Input tokens", short(total_in)))
                    .child(tile("Output tokens", short(total_out)))
                    .child(tile("Turns", turns.to_string()))
                    .into_any_element(),
                heading("Tokens per day", cx).into_any_element(),
                card(cx)
                    .p_4()
                    .child(if daily.is_empty() {
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("No turns in this period.")
                            .into_any_element()
                    } else {
                        bars.into_any_element()
                    })
                    .into_any_element(),
                heading("By CLI", cx).into_any_element(),
                table.into_any_element(),
                div()
                    .px_1()
                    .pt_1()
                    .text_xs()
                    .text_color(muted)
                    .child("Counted from what each CLI reports when a turn ends. Subscription limits stay in each vendor's own app.")
                    .into_any_element(),
            ],
        )
    }
}

/// What to run to install a provider's CLI.
fn install_command(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude if cfg!(windows) => "irm https://claude.ai/install.ps1 | iex",
        Provider::Claude => "curl -fsSL https://claude.ai/install.sh | bash",
        Provider::Codex => "npm install -g @openai/codex",
        Provider::Cursor if cfg!(windows) => "irm 'https://cursor.com/install?win32=true' | iex",
        Provider::Cursor => "curl https://cursor.com/install -fsS | bash",
        Provider::Gemini => "npm install -g @google/gemini-cli",
        Provider::OpenCode => "npm install -g opencode-ai",
    }
}

fn short_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        other => other.label(),
    }
}

/// 3610000 as "3.6M".
fn short(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

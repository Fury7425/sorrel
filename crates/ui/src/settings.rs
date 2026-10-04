//! Settings pages, the project page and the usage page.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, Textarea},
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::{
    Access, AuthState, OpenIn, ProjectId, Provider, ProviderConfig, Request, Theme, TurnSettings,
};

use crate::sidebar::{ago, now};
use crate::style::{
    ACCENTS, SIDEBAR, card, heading, parse_hex, provider_dot, row, segment, segmented,
};
use crate::{Section, View, Workspace};

impl Section {
    const ALL: [Section; 7] = [
        Section::General,
        Section::Appearance,
        Section::Providers,
        Section::Connectors,
        Section::Tasks,
        Section::Archived,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Providers => "Providers",
            Section::Connectors => "Connectors",
            Section::Tasks => "Scheduled tasks",
            Section::Archived => "Archived",
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

/// A scrollable page with a large title.
fn page(id: &'static str, title: impl Into<SharedString>) -> Stateful<Div> {
    v_flex()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .items_center()
        .child(
            v_flex()
                .w_full()
                .max_w(px(800.))
                .px_6()
                .pt_5()
                .pb_12()
                .gap_2()
                .child(
                    div()
                        .px_1()
                        .pb_1()
                        .text_2xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.into()),
                ),
        )
}

/// Adds children to a page's inner column.
trait PageExt {
    fn body(self, children: impl IntoIterator<Item = AnyElement>) -> Self;
}

impl PageExt for Stateful<Div> {
    fn body(self, children: impl IntoIterator<Item = AnyElement>) -> Self {
        // The column is the page's only child; rebuilding it keeps the API small.
        self.child(
            v_flex()
                .w_full()
                .max_w(px(800.))
                .px_6()
                .pb_12()
                .gap_2()
                .children(children),
        )
    }
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
        v_flex()
            .w(px(SIDEBAR))
            .h_full()
            .flex_shrink_0()
            .gap_0p5()
            .p_2()
            .bg(if see_through {
                sidebar.opacity(0.55)
            } else {
                sidebar
            })
            .border_r_1()
            .border_color(fg.opacity(0.07))
            .child(
                Button::new("settings-back")
                    .ghost()
                    .small()
                    .w_full()
                    .justify_start()
                    .icon(IconName::ArrowLeft)
                    .label("Back")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::Home;
                        cx.notify();
                    })),
            )
            .child(div().h(px(8.)))
            .children(Section::ALL.iter().enumerate().map(|(ix, &section)| {
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
                        el.bg(fg.opacity(0.09)).font_weight(FontWeight::SEMIBOLD)
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
            }))
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
        let enabled: Vec<Provider> = self
            .settings
            .providers
            .iter()
            .filter(|(_, c)| !c.disabled)
            .map(|(p, _)| *p)
            .collect();
        let provider = segmented(cx).children(enabled.into_iter().map(|p| {
            segment(
                cx,
                ("default-provider", p as usize),
                short_label(p),
                prefs.provider == p,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_prefs(
                    |prefs| {
                        prefs.provider = p;
                        prefs.settings = TurnSettings {
                            model: None,
                            effort: None,
                            fast: false,
                            long_context: false,
                            ..prefs.settings.clone()
                        };
                    },
                    cx,
                )
            }))
        }));
        let access = segmented(cx).children(
            [
                (Access::Supervised, "Supervised"),
                (Access::AutoEdits, "Edits"),
                (Access::Auto, "Auto"),
                (Access::FullAccess, "Full"),
            ]
            .map(|(value, label)| {
                segment(
                    cx,
                    ("default-access", value as usize),
                    label,
                    prefs.settings.access == value,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_prefs(|p| p.settings.access = value, cx)
                }))
            }),
        );
        let stepper = h_flex()
            .gap_1()
            .child(
                Button::new("sessions-down")
                    .outline()
                    .xsmall()
                    .icon(IconName::Minus)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::SetMaxSessions {
                            count: max.saturating_sub(1),
                        })
                    })),
            )
            .child(
                div()
                    .w(px(28.))
                    .text_center()
                    .text_sm()
                    .child(max.to_string()),
            )
            .child(
                Button::new("sessions-up")
                    .outline()
                    .xsmall()
                    .icon(IconName::Plus)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.request(Request::SetMaxSessions { count: max + 1 })
                    })),
            );
        let follow = segmented(cx)
            .child(
                segment(cx, "follow-queue", "Queue", !prefs.enter_steers).on_click(
                    cx.listener(|this, _, _, cx| this.set_prefs(|p| p.enter_steers = false, cx)),
                ),
            )
            .child(
                segment(cx, "follow-steer", "Steer", prefs.enter_steers).on_click(
                    cx.listener(|this, _, _, cx| this.set_prefs(|p| p.enter_steers = true, cx)),
                ),
            );
        let updates = Switch::new("check-updates")
            .checked(prefs.check_updates)
            .on_click(cx.listener(|this, on: &bool, _, cx| {
                let on = *on;
                this.set_prefs(|p| p.check_updates = on, cx)
            }));

        page("general", "General")
            .body([
                heading("Start", cx).into_any_element(),
                card(cx)
                    .child(row("Open in", "Chat is a plain conversation. Code works in a project folder with tools, files and approvals. Switch any time at the top of the sidebar.", open_in, false, cx))
                    .child(row("New threads use", "The CLI a new thread starts on. The model chip in a thread can change it.", provider, false, cx))
                    .child(row("Permissions", "How much a new Code thread may do before it asks.", access, false, cx))
                    .child(row("Sessions at once", "Turns beyond this wait for a free slot.", stepper, true, cx))
                    .into_any_element(),
                heading("Behavior", cx).into_any_element(),
                card(cx)
                    .child(row("Enter while the agent works", "Queue runs the message next. Steer folds it into the running turn.", follow, false, cx))
                    .child(row("Check for updates", "Look for a new Sorrel release on start.", updates, true, cx))
                    .into_any_element(),
                heading("Memory", cx).into_any_element(),
                card(cx)
                    .p_4()
                    .gap_3()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Shared with every session through CLAUDE.md and AGENTS.md."))
                    .child(Textarea::new(&self.editors.memory))
                    .child(h_flex().child(
                        Button::new("save-memory").outline().small().label("Save memory").on_click(
                            cx.listener(|this, _, _, cx| {
                                let text = this.editors.memory.read(cx).value().to_string();
                                this.request(Request::SetMemory { text });
                            }),
                        ),
                    ))
                    .into_any_element(),
            ])
            .into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let prefs = self.prefs().clone();
        let theme = cx.theme();
        let (fg, muted, primary) = (theme.foreground, theme.muted_foreground, theme.primary);
        let scheme = segmented(cx).children(
            [
                (Theme::System, "System"),
                (Theme::Light, "Light"),
                (Theme::Dark, "Dark"),
            ]
            .map(|(value, label)| {
                segment(cx, ("theme", value as usize), label, prefs.theme == value).on_click(
                    cx.listener(move |this, _, _, cx| this.set_prefs(|p| p.theme = value, cx)),
                )
            }),
        );
        let swatches = h_flex()
            .gap_2()
            .children(ACCENTS.iter().enumerate().map(|(ix, hex)| {
                let color = parse_hex(hex).unwrap_or(primary);
                let on = prefs.accent == *hex;
                div()
                    .id(("accent", ix))
                    .size(px(24.))
                    .rounded_lg()
                    .cursor_pointer()
                    .bg(color)
                    .when(hex.is_empty(), |el| {
                        el.border_2().border_color(fg.opacity(0.3))
                    })
                    .when(on, |el| el.border_2().border_color(fg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_prefs(|p| p.accent = hex.to_string(), cx)
                    }))
            }));
        let glass = Switch::new("glass")
            .checked(prefs.glass)
            .on_click(cx.listener(|this, on: &bool, _, cx| {
                let on = *on;
                this.set_prefs(|p| p.glass = on, cx)
            }));
        let scanlines = Switch::new("scanlines")
            .checked(prefs.scanlines)
            .disabled(prefs.wallpaper.is_empty())
            .on_click(cx.listener(|this, on: &bool, _, cx| {
                let on = *on;
                this.set_prefs(|p| p.scanlines = on, cx)
            }));
        let has_wallpaper = !prefs.wallpaper.is_empty();
        let name = std::path::Path::new(&prefs.wallpaper)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "None".into());
        let wallpaper = h_flex()
            .gap_3()
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
                            .child("Wallpaper"),
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
                        .label("Remove")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_prefs(|p| p.wallpaper.clear(), cx)
                        })),
                )
            });

        page("appearance", "Appearance")
            .body([
                heading("Color", cx).into_any_element(),
                card(cx)
                    .child(row(
                        "Color scheme",
                        "System follows the OS.",
                        scheme,
                        false,
                        cx,
                    ))
                    .child(row(
                        "Accent",
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
                        "A see-through, blurred window where the OS supports it. Off is solid.",
                        glass,
                        false,
                        cx,
                    ))
                    .child(wallpaper)
                    .child(row(
                        "Scanlines",
                        "A display-line texture over the wallpaper.",
                        scanlines,
                        true,
                        cx,
                    ))
                    .into_any_element(),
            ])
            .into_any_element()
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, danger, success) = (
            theme.foreground,
            theme.muted_foreground,
            theme.danger,
            theme.success,
        );
        let selected = self.provider_page;
        let config_of = |p: Provider| {
            self.settings
                .providers
                .iter()
                .find(|(x, _)| *x == p)
                .map(|(_, c)| c.clone())
                .unwrap_or_default()
        };
        let status_of = |p: Provider| {
            self.auth
                .iter()
                .find(|a| a.provider == p)
                .map(|a| (a.state, a.detail.clone()))
                .unwrap_or((AuthState::Unknown, String::new()))
        };
        let state_text = |state: AuthState| match state {
            AuthState::Unknown => "Unknown",
            AuthState::Missing => "Not installed",
            AuthState::SignedOut => "Signed out",
            AuthState::Subscription => "Signed in",
            AuthState::ApiKey => "API key",
        };
        let state_color = |state: AuthState| match state {
            AuthState::Subscription | AuthState::ApiKey => success,
            AuthState::Missing | AuthState::SignedOut => danger,
            AuthState::Unknown => muted,
        };

        let list = v_flex()
            .w(px(240.))
            .flex_shrink_0()
            .border_r_1()
            .border_color(fg.opacity(0.06))
            .children(Provider::ALL.iter().map(|&p| {
                let config = config_of(p);
                let (state, _) = status_of(p);
                h_flex()
                    .id(("provider", p as usize))
                    .gap_2p5()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(fg.opacity(0.05))
                    .cursor_pointer()
                    .when(p == selected, |el| el.bg(fg.opacity(0.05)))
                    .child(provider_dot(p, 10.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when(config.disabled, |el| el.text_color(muted))
                                    .child(p.label()),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(div().size(px(6.)).rounded_full().bg(
                                        if config.disabled {
                                            muted
                                        } else {
                                            state_color(state)
                                        },
                                    ))
                                    .child(if config.disabled {
                                        "Off"
                                    } else {
                                        state_text(state)
                                    }),
                            ),
                    )
                    .child(
                        Switch::new(("provider-on", p as usize))
                            .checked(!config.disabled)
                            .on_click(cx.listener(move |this, on: &bool, _, _| {
                                let mut config = this
                                    .settings
                                    .providers
                                    .iter()
                                    .find(|(x, _)| *x == p)
                                    .map(|(_, c)| c.clone())
                                    .unwrap_or_default();
                                config.disabled = !*on;
                                this.request(Request::SetProviderConfig {
                                    provider: p,
                                    config,
                                });
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.provider_page = p;
                        if !this.catalog.contains_key(&p) {
                            this.request(Request::ListModels { provider: p });
                        }
                        cx.notify();
                    }))
            }));

        let config = config_of(selected);
        let (state, detail) = status_of(selected);
        let key_set = self.settings.api_key_set.contains(&selected);
        let using_key = self.settings.use_api_key.contains(&selected);
        let editor = |list: &Vec<(Provider, Entity<gpui_kit::component::input::InputState>)>| {
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

        let account = card(cx)
            .child(row(
                "Account",
                format!("{} · {}", state_text(state), detail),
                h_flex()
                    .gap_1()
                    .child(Button::new("check-auth").ghost().small().icon(IconName::RefreshCw).tooltip("Check again").on_click(
                        cx.listener(|this, _, _, _| this.request(Request::CheckAuth)),
                    ))
                    .child(Button::new("sign-in").outline().small().label("Sign in").on_click(
                        cx.listener(move |this, _, _, _| this.request(Request::SignIn { provider: selected })),
                    )),
                false,
                cx,
            ))
            .child(row(
                "API key",
                "Stored on this machine and handed to the CLI. Subscription sign-in stays inside the CLI.",
                h_flex()
                    .gap_1()
                    .children(key.map(|key| div().w(px(180.)).child(Input::new(&key).small())))
                    .child(Button::new("save-key").outline().small().label("Save").on_click(cx.listener(
                        move |this, _, window, cx| {
                            if let Some((_, key)) = this.editors.keys.iter().find(|(p, _)| *p == selected).cloned() {
                                let value = key.read(cx).value().to_string();
                                key.update(cx, |e, cx| e.set_value("", window, cx));
                                this.request(Request::SetApiKey { provider: selected, key: Some(value) });
                            }
                        },
                    )))
                    .when(key_set, |el| {
                        el.child(Switch::new("use-key").checked(using_key).tooltip("Use the API key").on_click(
                            cx.listener(move |this, on: &bool, _, _| {
                                this.request(Request::SetUseApiKey { provider: selected, on: *on })
                            }),
                        ))
                        .child(Button::new("remove-key").ghost().small().icon(IconName::Close).tooltip("Remove key").on_click(
                            cx.listener(move |this, _, _, _| {
                                this.request(Request::SetApiKey { provider: selected, key: None })
                            }),
                        ))
                    }),
                true,
                cx,
            ));

        let save_runtime = Button::new("save-runtime")
            .outline()
            .small()
            .label("Save")
            .on_click(cx.listener(move |this, _, _, cx| {
                let read =
                    |list: &Vec<(Provider, Entity<gpui_kit::component::input::InputState>)>,
                     cx: &App| {
                        list.iter()
                            .find(|(p, _)| *p == selected)
                            .map(|(_, e)| e.read(cx).value().trim().to_string())
                            .unwrap_or_default()
                    };
                let mut config = this
                    .settings
                    .providers
                    .iter()
                    .find(|(x, _)| *x == selected)
                    .map(|(_, c)| c.clone())
                    .unwrap_or_default();
                config.binary = read(&this.editors.binaries, cx);
                config.args = read(&this.editors.args, cx);
                this.request(Request::SetProviderConfig {
                    provider: selected,
                    config,
                });
            }));
        let runtime = card(cx)
            .child(row(
                "Binary path",
                "Leave empty to find it on PATH.",
                div()
                    .w(px(260.))
                    .children(binary.map(|b| Input::new(&b).small())),
                false,
                cx,
            ))
            .child(row(
                "Launch arguments",
                "Added when a session starts.",
                div()
                    .w(px(260.))
                    .children(args.map(|a| Input::new(&a).small())),
                false,
                cx,
            ))
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .child(div().flex_1())
                    .child(save_runtime),
            );

        let env_rows = config.env.iter().enumerate().map(|(ix, (k, v))| {
            let key = k.clone();
            h_flex()
                .gap_3()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(fg.opacity(0.05))
                .text_sm()
                .font_family(cx.theme().mono_font_family.clone())
                .child(div().w(px(240.)).truncate().child(k.clone()))
                .child(div().flex_1().truncate().text_color(muted).child(v.clone()))
                .child(
                    Button::new(("remove-env", ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(cx.listener(move |this, _, _, _| {
                            let mut config = this
                                .settings
                                .providers
                                .iter()
                                .find(|(x, _)| *x == selected)
                                .map(|(_, c)| c.clone())
                                .unwrap_or_default();
                            config.env.retain(|(k, _)| *k != key);
                            this.request(Request::SetProviderConfig {
                                provider: selected,
                                config,
                            });
                        })),
                )
                .into_any_element()
        });
        let environment = card(cx).children(env_rows).child(
            h_flex()
                .gap_2()
                .px_4()
                .py_2p5()
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
                        .outline()
                        .small()
                        .icon(IconName::Plus)
                        .label("Add")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let key = this.editors.env_key.read(cx).value().trim().to_string();
                            let value = this.editors.env_value.read(cx).value().to_string();
                            if key.is_empty() {
                                return;
                            }
                            let mut config: ProviderConfig = this
                                .settings
                                .providers
                                .iter()
                                .find(|(x, _)| *x == selected)
                                .map(|(_, c)| c.clone())
                                .unwrap_or_default();
                            config.env.retain(|(k, _)| *k != key);
                            config.env.push((key, value));
                            this.editors
                                .env_key
                                .update(cx, |e, cx| e.set_value("", window, cx));
                            this.editors
                                .env_value
                                .update(cx, |e, cx| e.set_value("", window, cx));
                            this.request(Request::SetProviderConfig {
                                provider: selected,
                                config,
                            });
                        })),
                ),
        );

        let model_chips = h_flex()
            .flex_wrap()
            .gap_2()
            .p_4()
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

        let detail = v_flex()
            .flex_1()
            .min_w_0()
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(provider_dot(selected, 10.))
                    .child(selected.label()),
            )
            .child(account)
            .child(heading("Runtime", cx))
            .child(runtime)
            .child(heading("Environment", cx))
            .child(environment)
            .child(heading("Models", cx))
            .child(card(cx).child(model_chips));

        page("providers", "Providers")
            .body([card(cx)
                .flex_row()
                .items_start()
                .child(list)
                .child(detail)
                .into_any_element()])
            .into_any_element()
    }

    fn render_connectors(&self, cx: &mut Context<Self>) -> AnyElement {
        page("connectors", "Connectors")
            .body([
                heading("MCP servers", cx).into_any_element(),
                card(cx)
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
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
            ])
            .into_any_element()
    }

    fn render_tasks(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let now = now();
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
            } else if task.next_run <= now {
                "due now".to_owned()
            } else {
                format!("next in {} min", (task.next_run - now + 59) / 60)
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
            .child(div().text_xs().text_color(muted).child("Prompts that run on their own and post results to a thread. Nobody is there to approve, so anything that needs approval is denied."))
            .child(Textarea::new(&self.editors.task_prompt))
            .child(projects)
            .child(providers)
            .child(Input::new(&self.editors.task_every).small())
            .child(h_flex().child(Button::new("create-task").primary().small().label("Create task").on_click(cx.listener(
                |this, _, window, cx| {
                    let prompt = this.editors.task_prompt.read(cx).value().to_string();
                    let every = this.editors.task_every.read(cx).value().trim().parse::<u32>().ok();
                    this.request(Request::CreateTask {
                        project: this.task_project,
                        provider: this.task_provider,
                        prompt,
                        every_minutes: every,
                    });
                    this.editors.task_prompt.update(cx, |e, cx| e.set_value("", window, cx));
                    this.editors.task_every.update(cx, |e, cx| e.set_value("", window, cx));
                },
            ))));
        page("tasks", "Scheduled tasks")
            .body(std::iter::once(form.into_any_element()).chain(list))
            .into_any_element()
    }

    fn render_archived(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let now = now();
        let archived: Vec<_> = self.threads.iter().filter(|t| t.archived).collect();
        let rows = archived.iter().enumerate().map(|(ix, t)| {
            let id = t.id;
            row(
                t.title.clone(),
                format!(
                    "{} · {}",
                    if t.chat { "Chat" } else { "Code" },
                    ago(now - t.updated_at)
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
        });
        let body = if archived.is_empty() {
            card(cx).p_4().child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Archived sessions and chats show up here."),
            )
        } else {
            card(cx).children(rows)
        };
        page("archived", "Archived")
            .body([body.into_any_element()])
            .into_any_element()
    }

    fn render_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        page("about", "About")
            .body([card(cx)
                .child(row("Version", format!("Sorrel {}", env!("CARGO_PKG_VERSION")), div(), false, cx))
                .child(row("Data folder", self.settings.data_dir.display().to_string(), div(), false, cx))
                .child(
                    div()
                        .px_4()
                        .py_3()
                        .text_xs()
                        .text_color(muted)
                        .child("Sorrel runs the official CLIs you already use. Sign-in stays inside each CLI; Sorrel never reads their credentials."),
                )
                .into_any_element()])
            .into_any_element()
    }

    pub(crate) fn render_project(&self, id: ProjectId, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(project) = self.projects.iter().find(|p| p.id == id) else {
            return page("project", "Project")
                .body([div()
                    .text_sm()
                    .child("This project was removed.")
                    .into_any_element()])
                .into_any_element();
        };
        let folder = project.folder.clone();
        page("project", project.name.clone())
            .body([card(cx)
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
                .into_any_element()])
            .into_any_element()
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
                .gap_1()
                .child(div().text_xs().text_color(muted).child(label))
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::MEDIUM)
                        .child(value),
                )
        };
        let peak = daily.iter().map(|d| d.1).max().unwrap_or(1).max(1);
        let bars = h_flex()
            .items_end()
            .gap_2()
            .h(px(160.))
            .children(daily.iter().map(|(day, tokens)| {
                let height = 140. * *tokens as f32 / peak as f32;
                v_flex()
                    .flex_1()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .w_full()
                            .h(px(height.max(2.)))
                            .rounded_t_md()
                            .bg(primary.opacity(0.8)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(ago(now() - day).replace("now", "today")),
                    )
            }));
        let table = card(cx).children(rows.iter().enumerate().map(|(ix, r)| {
            h_flex()
                .gap_3()
                .px_4()
                .py_2p5()
                .when(ix + 1 < rows.len(), |el| {
                    el.border_b_1().border_color(fg.opacity(0.05))
                })
                .text_sm()
                .child(provider_dot(r.provider, 8.))
                .child(div().flex_1().child(r.provider.label()))
                .child(div().w(px(110.)).child(format!("{} in", short(r.input))))
                .child(div().w(px(110.)).child(format!("{} out", short(r.output))))
                .child(
                    div()
                        .w(px(90.))
                        .text_color(muted)
                        .child(format!("{} turns", r.turns)),
                )
                .child(
                    div()
                        .w(px(90.))
                        .text_color(muted)
                        .child(format!("{} threads", r.threads)),
                )
                .into_any_element()
        }));
        page("usage", "Usage")
            .body([
                h_flex().child(range).into_any_element(),
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
                        div().text_sm().text_color(muted).child("No turns in this period.").into_any_element()
                    } else {
                        bars.into_any_element()
                    })
                    .into_any_element(),
                heading("By CLI", cx).into_any_element(),
                table.into_any_element(),
                div()
                    .px_1()
                    .text_xs()
                    .text_color(muted)
                    .child("Counted from what each CLI reports when a turn ends. Subscription limits stay in each vendor's own app.")
                    .into_any_element(),
            ])
            .into_any_element()
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

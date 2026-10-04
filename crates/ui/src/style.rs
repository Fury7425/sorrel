//! Shared look: setting cards and rows, segmented controls, provider marks,
//! the accent color, the wallpaper layer, and two motion pieces drawn
//! natively after libraries.dev's border beam and thinking orb.

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use proto::Provider;

/// Swatches on the Appearance page; empty is the theme's own accent.
pub const ACCENTS: [&str; 8] = [
    "", "#8b7cf6", "#f08c3c", "#e8b530", "#3fbf6a", "#2fc5d8", "#5b9cf5", "#ec6fae",
];

/// Width of the sidebar and of the title bar's matching left part.
pub const SIDEBAR: f32 = 252.;

/// `#rrggbb` as a color.
pub fn parse_hex(hex: &str) -> Option<Hsla> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(rgb(value).into())
}

/// Paints `hex` over the theme's primary colors; empty leaves the theme alone.
pub fn apply_accent(hex: &str, cx: &mut App) {
    let Some(color) = parse_hex(hex) else { return };
    Theme::update(cx, |theme| {
        let c = &mut theme.colors;
        c.primary = color;
        c.primary_hover = color.opacity(0.9);
        c.primary_active = color.opacity(0.8);
        c.button_primary = color;
        c.button_primary_hover = color.opacity(0.9);
        c.button_primary_active = color.opacity(0.8);
        c.ring = color;
        c.caret = color;
    });
}

/// Each CLI's mark color, used where a logo would go.
pub fn provider_color(provider: Provider) -> Hsla {
    rgb(match provider {
        Provider::Claude => 0xd97757,
        Provider::Codex => 0xc9c9cf,
        Provider::Cursor => 0xa0a0a8,
        Provider::Gemini => 0x6aa7ff,
        Provider::OpenCode => 0x9a9aa2,
    })
    .into()
}

/// The provider's mark in its color. These are Sorrel's own abstract marks,
/// not the vendors' logos.
pub fn provider_icon(provider: Provider, size: f32) -> Svg {
    svg()
        .path(match provider {
            Provider::Claude => "icons/provider-claude.svg",
            Provider::Codex => "icons/provider-codex.svg",
            Provider::Cursor => "icons/provider-cursor.svg",
            Provider::Gemini => "icons/provider-gemini.svg",
            Provider::OpenCode => "icons/provider-opencode.svg",
        })
        .size(px(size))
        .flex_shrink_0()
        .text_color(provider_color(provider))
}

/// The mark on a tinted rounded square, for lists and avatars.
pub fn provider_tile(provider: Provider, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded(px(size * 0.28))
        .bg(provider_color(provider).opacity(0.16))
        .flex()
        .items_center()
        .justify_center()
        .child(provider_icon(provider, size * 0.6))
}

/// A small filled circle in the provider's color.
pub fn provider_dot(provider: Provider, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .bg(provider_color(provider))
}

/// A frosted pane: translucent fill, hairline border, faint top highlight.
pub fn glass(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .bg(theme.background.opacity(0.62))
        .border_1()
        .border_color(theme.foreground.opacity(0.09))
        .rounded_xl()
}

/// A rounded group of setting rows.
pub fn card(cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .w_full()
        .rounded_xl()
        .border_1()
        .border_color(theme.foreground.opacity(0.07))
        .bg(theme.background.opacity(0.5))
}

/// A section heading above a card.
pub fn heading(text: &'static str, cx: &App) -> Div {
    div()
        .pt_4()
        .pb_1()
        .px_1()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// One setting: title and description on the left, its control on the right.
pub fn row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    last: bool,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let description: SharedString = description.into();
    h_flex()
        .gap_6()
        .px_4()
        .py_3()
        .when(!last, |el| {
            el.border_b_1().border_color(theme.foreground.opacity(0.05))
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.into()),
                )
                .when(!description.is_empty(), |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(description),
                    )
                }),
        )
        .child(control)
}

/// The container of a segmented control; children come from [`segment`].
pub fn segmented(cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_shrink_0()
        .gap_0p5()
        .p_0p5()
        .rounded_lg()
        .bg(theme.foreground.opacity(0.05))
        .border_1()
        .border_color(theme.foreground.opacity(0.08))
}

/// One choice in a segmented control; the chosen one gets a raised fill.
pub fn segment(
    cx: &App,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
) -> Button {
    let theme = cx.theme();
    let (fg, muted) = (theme.foreground, theme.muted_foreground);
    Button::new(id)
        .ghost()
        .xsmall()
        .label(label.into())
        .selected(on)
        .when(on, |b| b.bg(fg.opacity(0.13)).text_color(fg))
        .when(!on, |b| b.text_color(muted))
}

/// How much of the wallpaper a screen shows.
#[derive(Clone, Copy, PartialEq)]
pub enum Backdrop {
    /// New thread or chat: the picture stays clear at the top and sinks into the dark.
    Hero,
    /// An open session: the same picture, dimmed behind the transcript.
    Session,
    /// Settings and pages: only its colors remain.
    Quiet,
}

/// The wallpaper behind the whole window.
pub fn wallpaper(path: &str, mode: Backdrop, scanlines: bool, background: Hsla) -> Div {
    let veil = match mode {
        Backdrop::Hero => div().absolute().inset_0().bg(linear_gradient(
            180.,
            linear_color_stop(background.opacity(0.0), 0.2),
            linear_color_stop(background.opacity(0.96), 0.62),
        )),
        Backdrop::Session => div().absolute().inset_0().bg(linear_gradient(
            180.,
            linear_color_stop(background.opacity(0.72), 0.0),
            linear_color_stop(background.opacity(0.9), 0.6),
        )),
        Backdrop::Quiet => div().absolute().inset_0().bg(background.opacity(0.88)),
    };
    let scrim = (mode == Backdrop::Hero).then(|| {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(110.))
            .bg(linear_gradient(
                180.,
                linear_color_stop(background.opacity(0.6), 0.),
                linear_color_stop(background.opacity(0.), 1.),
            ))
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .child(
            img(std::path::PathBuf::from(path))
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .object_fit(ObjectFit::Cover),
        )
        .child(veil)
        .children(scrim)
        .when(scanlines, |el| el.child(scanline_layer()))
}

/// Thin dark lines every few pixels, painted live so they stay one screen
/// pixel thick whatever the wallpaper's size; about 300 quads.
fn scanline_layer() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |bounds, _, window, _| {
            let line = hsla(0., 0., 0., 0.18);
            let mut y = bounds.top();
            while y < bounds.bottom() {
                window.paint_quad(fill(
                    Bounds::new(point(bounds.left(), y), size(bounds.size.width, px(1.))),
                    line,
                ));
                y += px(3.);
            }
        },
    )
    .absolute()
    .inset_0()
}

/// A light that travels around an element's border while work is under
/// way (after libraries.dev's border beam, `md` type). Place it as the last
/// child of a `relative` element; it draws only while mounted, so mount it
/// only while busy and the window stays idle otherwise.
pub fn border_beam(id: impl Into<ElementId>, color: Hsla) -> impl IntoElement {
    div().absolute().inset_0().with_animation(
        id,
        Animation::new(Duration::from_millis(3200)).repeat(),
        move |el, t| el.child(beam_canvas(t, color)),
    )
}

fn beam_canvas(t: f32, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let inset = 6.;
            let perimeter = 2. * (w + h - 4. * inset);
            if perimeter <= 0. {
                return;
            }
            // A point `d` along the border, clockwise from the top-left corner.
            let at = |d: f32| {
                let d = d.rem_euclid(perimeter);
                let (top, right, bottom) = (w - 2. * inset, h - 2. * inset, w - 2. * inset);
                if d < top {
                    (inset + d, 0.)
                } else if d < top + right {
                    (w, inset + d - top)
                } else if d < top + right + bottom {
                    (w - inset - (d - top - right), h)
                } else {
                    (0., h - inset - (d - top - right - bottom))
                }
            };
            let head = t * perimeter;
            const TRAIL: usize = 90;
            for i in 0..TRAIL {
                let fade = 1. - i as f32 / TRAIL as f32;
                let (x, y) = at(head - i as f32 * 1.6);
                for (radius, alpha) in [(6., 0.03), (1.6, 0.5)] {
                    let r = radius * (0.5 + 0.5 * fade);
                    window.paint_quad(
                        fill(
                            Bounds::new(
                                point(bounds.left() + px(x - r), bounds.top() + px(y - r)),
                                size(px(2. * r), px(2. * r)),
                            ),
                            color.opacity(alpha * fade),
                        )
                        .corner_radii(px(r)),
                    );
                }
            }
        },
    )
    .size_full()
}

/// A small dotted orb that turns while an agent works (after libraries.dev's
/// thinking orb). Mount it only while busy.
pub fn thinking_orb(id: impl Into<ElementId>, diameter: f32, color: Hsla) -> impl IntoElement {
    div().size(px(diameter)).flex_shrink_0().with_animation(
        id,
        Animation::new(Duration::from_millis(2400)).repeat(),
        move |el, t| el.child(orb_canvas(t, color)),
    )
}

fn orb_canvas(t: f32, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let d = f32::from(bounds.size.width);
            let (cx, cy, r) = (d / 2., d / 2., d / 2. - 1.5);
            let spin = t * std::f32::consts::TAU;
            // Dots on a sphere: rings of latitude, each turned by the spin.
            for ring in 1..5 {
                let lat = ring as f32 / 5. * std::f32::consts::PI - std::f32::consts::FRAC_PI_2;
                let count = 6 + ring % 2 * 2;
                for k in 0..count {
                    let lon = k as f32 / count as f32 * std::f32::consts::TAU + spin;
                    let (x, z) = (lat.cos() * lon.cos(), lat.cos() * lon.sin());
                    let y = lat.sin();
                    let depth = (z + 1.) / 2.;
                    let dot = 0.8 + 1.1 * depth;
                    window.paint_quad(
                        fill(
                            Bounds::new(
                                point(
                                    bounds.left() + px(cx + x * r - dot / 2.),
                                    bounds.top() + px(cy + y * r - dot / 2.),
                                ),
                                size(px(dot), px(dot)),
                            ),
                            color.opacity(0.25 + 0.75 * depth),
                        )
                        .corner_radii(px(dot / 2.)),
                    );
                }
            }
        },
    )
    .size_full()
}

/// The color Ultrathink cycles through: blue, violet, pink and back.
pub fn ultra_hue(t: f32) -> Hsla {
    // Hue 0.58 (blue) to 0.92 (pink) and back.
    let wave = 0.5 - 0.5 * (t * std::f32::consts::TAU).cos();
    hsla(0.58 + 0.34 * wave, 0.85, 0.72, 1.)
}

//! Floating, detached activity panel mirroring the left sidebar: catch-up summary,
//! mentions and agent status. Hidden below 1200px.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyView, BoxShadow, Context, Entity, FontWeight, Hsla,
    SharedString, StyleRefinement, Window, div, linear_color_stop, linear_gradient, point,
    prelude::*, px, relative, transparent_black,
};
use model::AgentStatus;
use ui::{ActiveTheme, IconName, Theme, Tooltip, brand_icon, icon, pill};

use crate::store::ChatStore;

pub struct RightSidebar {
    store: Entity<ChatStore>,
    catch_up: Entity<CatchUp>,
    mentions_open: bool,
}

impl RightSidebar {
    pub fn new(store: Entity<ChatStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            catch_up: cx.new(|_| CatchUp),
            mentions_open: true,
        }
    }
}

fn section(t: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .rounded(px(21.))
        .border_1()
        .border_color(t.line)
        .bg(t.sunken)
}

impl Render for RightSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let ws = self.store.read(cx).workspace();

        let mention_rows: Vec<_> = ws
            .mentions
            .iter()
            .map(|m| {
                let agent = ws.agent(m.agent);
                let name = agent.map_or("Agent".into(), |a| a.name.clone());
                (m.channel, agent.map(|a| a.brand), name, m.text.clone())
            })
            .collect();
        let agents: Vec<_> = ws
            .agents
            .iter()
            .map(|a| {
                (
                    a.brand,
                    a.status,
                    format!("{}: {}", a.name, a.status.label()),
                )
            })
            .collect();
        let count = mention_rows.len();
        let open = self.mentions_open;

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(6.))
            .rounded(px(26.))
            .border_1()
            .border_color(t.line)
            .bg(t.surface)
            .shadow(t.float_shadow())
            // Catch-up lives in its own cached view: its animation re-renders only it.
            .child(div().flex_shrink().min_h(px(148.)).h(px(280.)).child(
                AnyView::from(self.catch_up.clone()).cached(StyleRefinement::default().size_full()),
            ))
            // Mentions
            .child(
                section(&t)
                    .gap(px(6.))
                    .p(px(6.))
                    .child(
                        pill("mentions-toggle", cx)
                            .h(px(36.))
                            .justify_center()
                            .gap(px(8.))
                            .text_color(t.ink_soft)
                            .tooltip(Tooltip::text(if open {
                                "Collapse mentions"
                            } else {
                                "Expand mentions"
                            }))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mentions_open = !this.mentions_open;
                                cx.notify();
                            }))
                            .when(!open, |p| {
                                p.child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!(
                                            "{count} {}",
                                            if count == 1 { "mention" } else { "mentions" }
                                        )),
                                )
                            })
                            .child(icon(
                                if open {
                                    IconName::CaretUp
                                } else {
                                    IconName::CaretDown
                                },
                                px(17.),
                                t.ink_soft,
                            )),
                    )
                    .when(open, |s| {
                        s.children(mention_rows.into_iter().enumerate().map(
                            |(ix, (channel, brand, name, text))| {
                                let tip = format!("{name}: {text}");
                                pill(("mention", ix), cx)
                                    .h(px(36.))
                                    .gap(px(8.))
                                    .pl(px(10.))
                                    .pr(px(12.))
                                    .text_size(px(13.5))
                                    .tooltip(Tooltip::text(tip))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.store.update(cx, |s, cx| s.select(channel, cx));
                                    }))
                                    .when_some(brand, |p, brand| {
                                        p.child(
                                            div()
                                                .size(px(20.))
                                                .flex_none()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(icon(brand_icon(brand), px(16.), t.ink)),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(SharedString::from(text)),
                                    )
                            },
                        ))
                    }),
            )
            // Agent status
            .child(
                section(&t)
                    .gap(px(8.))
                    .px(px(6.))
                    .pt(px(10.))
                    .pb(px(10.))
                    .child(
                        div()
                            .px(px(8.))
                            .text_size(px(13.))
                            .line_height(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.ink_soft)
                            .child("Agent Status"),
                    )
                    .child(div().flex().flex_col().gap(px(10.)).children(
                        agents.chunks(4).enumerate().map(|(row, chunk)| {
                            div()
                                .flex()
                                .gap(px(6.))
                                .children(chunk.iter().enumerate().map(
                                    |(col, (brand, status, label))| {
                                        let glow = match status {
                                            AgentStatus::Working => t.status_working,
                                            AgentStatus::Idle => t.status_idle,
                                            AgentStatus::Blocked => t.status_blocked,
                                        };
                                        pill(("agent", row * 4 + col), cx)
                                            .flex_1()
                                            .min_w_0()
                                            .h(px(44.))
                                            .justify_center()
                                            .shadow(t.glow_shadow(glow))
                                            .tooltip(Tooltip::text(label.clone()))
                                            .child(icon(brand_icon(*brand), px(19.), t.ink))
                                    },
                                ))
                        }),
                    )),
            )
            .child(div().flex_1())
    }
}

/// "Summarizing what happened while you were gone…" with a shimmering, blurred
/// stand-in for the summary text. Purely decorative until summaries exist.
pub struct CatchUp;

/// Placeholder line widths, eyeballed from the wireframe.
const SKELETON: [f32; 11] = [
    0.82, 0.90, 0.76, 0.97, 0.64, 0.74, 0.88, 0.79, 0.91, 0.52, 0.75,
];

fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    Hsla {
        h: a.h + (b.h - a.h) * t,
        s: a.s + (b.s - a.s) * t,
        l: a.l + (b.l - a.l) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

impl Render for CatchUp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        let period = Duration::from_millis(2400);

        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(21.))
            .border_1()
            .border_color(t.line)
            .bg(t.sunken)
            .px(px(14.))
            .pt(px(12.))
            .pb(px(12.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .child(
                        div()
                            .mt(px(1.))
                            .child(icon(IconName::Sparkle, px(19.), t.ink_soft))
                            .with_animation(
                                "catch-up-sparkle",
                                Animation::new(period).repeat(),
                                |el, delta| {
                                    // 1 -> 0.45 -> 1, eased.
                                    let wave = (delta * std::f32::consts::TAU).cos() * 0.5 + 0.5;
                                    el.opacity(0.45 + 0.55 * wave)
                                },
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(15.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.ink)
                            .child("Summarizing what happened while you were gone…"),
                    ),
            )
            .child(
                div()
                    .relative()
                    .mt(px(12.))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .px(px(2.))
                            .pt(px(2.))
                            .children(SKELETON.iter().enumerate().map(|(ix, w)| {
                                let (base, shine) = (t.skeleton, t.skeleton_shine);
                                let phase = ix as f32 * 0.029;
                                // GPUI has no blur filter: a soft shadow in the same
                                // colour stands in for the CSS blur(3px).
                                div()
                                    .h(px(13.))
                                    .flex_none()
                                    .w(relative(*w))
                                    .rounded_full()
                                    .with_animation(
                                        ("skeleton", ix),
                                        Animation::new(period).repeat(),
                                        move |el, delta| {
                                            let x = (delta - phase).rem_euclid(1.0);
                                            let glow = (x * std::f32::consts::PI).sin().powi(2);
                                            let color = mix(base, shine, glow);
                                            el.bg(color.opacity(0.75)).shadow(vec![BoxShadow {
                                                color,
                                                offset: point(px(0.), px(0.)),
                                                blur_radius: px(4.),
                                                spread_radius: px(0.),
                                            }])
                                        },
                                    )
                            })),
                    )
                    // Fade the overflow so a short panel cuts off softly.
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(24.))
                            .bg(linear_gradient(
                                180.,
                                linear_color_stop(transparent_black(), 0.),
                                linear_color_stop(t.sunken, 1.),
                            )),
                    ),
            )
    }
}

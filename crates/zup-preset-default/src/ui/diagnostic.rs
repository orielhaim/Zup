//! What went wrong, and what a person can do about it.

// GPUI's derive macros emit paths rooted at gpui_kit rather than at the crate
// that re-exports it, so a module deriving one has to be able to name that
// crate. Aliased through the SDK so it is the version this preset builds with.
#[allow(unused_imports)]
use zup_preset_sdk::gpui_kit;

use zup_preset_sdk::gpui_kit::assets::IconName;
use zup_preset_sdk::gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use zup_preset_sdk::gpui_kit::prelude::FluentBuilder as _;
use zup_preset_sdk::gpui_kit::{
    AnyElement, App, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, relative,
};
use zup_preset_sdk::host::DiagnosticKind;

use crate::model::{self, Severity};
use crate::theme::{size, space, text};
use crate::ui::{Callout, Disclosure, Handler, Tone, muted, title};

/// The badge a problem's title sits beside.
fn emblem(icon: IconName, color: Hsla, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .size(size::ICON_TILE * 1.5)
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(color.opacity(if theme.is_dark() { 0.16 } else { 0.1 }))
        .child(Icon::new(icon).size(size::ICON_LG).text_color(color))
}

/// A failure, explained the way the host diagnosed it.
///
/// What it means and what to do come first; the engine's own words are behind
/// a disclosure, for the person who will paste them into a support request.
#[derive(IntoElement)]
pub struct DiagnosticView {
    problem: model::Problem,
    technical_open: bool,
    on_toggle_technical: Handler,
    support: Vec<AnyElement>,
}

impl DiagnosticView {
    pub fn new(
        problem: model::Problem,
        technical_open: bool,
        on_toggle_technical: Handler,
    ) -> Self {
        Self {
            problem,
            technical_open,
            on_toggle_technical,
            support: Vec::new(),
        }
    }

    /// A support action: copying the details, opening the log.
    pub fn support(mut self, action: impl IntoElement) -> Self {
        self.support.push(action.into_any_element());
        self
    }
}

impl RenderOnce for DiagnosticView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let problem = self.problem;
        let (icon, color) = match (problem.severity, problem.kind) {
            (Severity::Recovery, _) | (_, DiagnosticKind::Recovery) => {
                (IconName::OctagonAlert, theme.danger)
            }
            (_, DiagnosticKind::Permission) => (IconName::ShieldAlert, theme.warning),
            (_, DiagnosticKind::Verification) => (IconName::ShieldAlert, theme.danger),
            (_, DiagnosticKind::Blocked | DiagnosticKind::Busy) => {
                (IconName::AppWindow, theme.warning)
            }
            (_, DiagnosticKind::Drift | DiagnosticKind::Conflict) => {
                (IconName::TriangleAlert, theme.warning)
            }
            (_, DiagnosticKind::Unknown) => (IconName::CircleX, theme.danger),
        };
        let recovery_tone = match problem.severity {
            Severity::Recovery => Tone::Negative,
            Severity::Failure => Tone::Neutral,
        };
        let technical = problem.technical.clone();
        v_flex()
            .gap(space::XL)
            .child(
                h_flex()
                    .items_start()
                    .gap(space::LG)
                    .child(emblem(icon, color, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(space::SM)
                            .pt(space::XS)
                            .child(title(problem.title.clone(), cx))
                            .child(muted(problem.meaning.clone(), cx)),
                    ),
            )
            .child(
                Callout::new(
                    recovery_tone,
                    match problem.severity {
                        Severity::Recovery => IconName::OctagonAlert,
                        Severity::Failure => IconName::Wrench,
                    },
                )
                .title(match problem.severity {
                    Severity::Recovery => "Before anything else",
                    Severity::Failure => "What you can do",
                })
                .body(problem.recovery.clone()),
            )
            .when_some(technical, |this, technical| {
                this.child(
                    Disclosure::new(
                        "technical-details",
                        "Technical details",
                        self.technical_open,
                        self.on_toggle_technical.clone(),
                    )
                    .child(
                        div()
                            .id("technical-text")
                            .max_h(space::XXXL * 4.)
                            .overflow_y_scroll()
                            .px(space::MD)
                            .py(space::SM)
                            .rounded(theme.radius)
                            .bg(theme.muted.opacity(0.6))
                            .border_1()
                            .border_color(theme.border.opacity(0.6))
                            .font_family(theme.mono_font_family.clone())
                            .text_size(text::CAPTION)
                            .line_height(relative(1.5))
                            .text_color(theme.foreground.opacity(0.85))
                            .child(technical),
                    ),
                )
            })
            .when(!self.support.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_wrap()
                        .gap(space::SM)
                        .ml(-space::SM)
                        .children(self.support),
                )
            })
    }
}

/// Applications holding files the operation needs.
#[derive(IntoElement)]
pub struct BlockedView {
    blocked: model::Blocked,
}

impl BlockedView {
    pub fn new(blocked: model::Blocked) -> Self {
        Self { blocked }
    }
}

impl RenderOnce for BlockedView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let blocked = self.blocked;
        v_flex()
            .gap(space::XL)
            .child(
                h_flex()
                    .items_start()
                    .gap(space::LG)
                    .child(emblem(IconName::AppWindow, theme.warning, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(space::SM)
                            .pt(space::XS)
                            .child(title(blocked.title.clone(), cx))
                            .child(muted(blocked.detail.clone(), cx)),
                    ),
            )
            .when(!blocked.apps.is_empty(), |this| {
                this.child(
                    v_flex()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .children(blocked.apps.iter().enumerate().map(|(index, app)| {
                            let name: SharedString = model::app_name(app).to_owned().into();
                            let detail = model::app_detail(app).map(str::to_owned);
                            h_flex()
                                .gap(space::MD)
                                .px(space::LG)
                                .py(space::MD)
                                .when(index > 0, |row| {
                                    row.border_t_1().border_color(theme.border.opacity(0.7))
                                })
                                .child(
                                    Icon::new(IconName::AppWindow)
                                        .size(size::ICON)
                                        .text_color(theme.muted_foreground),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(text::BODY)
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.foreground)
                                        .child(name),
                                )
                                .children(detail.map(|detail| {
                                    div()
                                        .flex_shrink_0()
                                        .text_size(text::SMALL)
                                        .text_color(theme.muted_foreground)
                                        .child(detail)
                                }))
                        })),
                )
            })
    }
}

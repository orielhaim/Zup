//! An operation in flight, and what it left behind.

use zup_preset_sdk::gpui_kit::assets::IconName;
use zup_preset_sdk::gpui_kit::component::progress::Progress;
use zup_preset_sdk::gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use zup_preset_sdk::gpui_kit::prelude::FluentBuilder as _;
use zup_preset_sdk::gpui_kit::{
    App, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    relative,
};

use crate::model;
use crate::theme::{size, space, text};
use crate::ui::{AppMark, Callout, MarkBadge, Tone, caption, muted, title};

/// The application's mark, what is happening, and how far along it is.
///
/// One bar and one sentence. The bar is determinate only when the engine can
/// count its work, because a bar that sits at zero while work happens says
/// something untrue.
#[derive(IntoElement)]
pub struct OperationProgress {
    product: SharedString,
    logo: Option<SharedString>,
    progress: model::Progress,
}

impl OperationProgress {
    pub fn new(
        product: impl Into<SharedString>,
        logo: Option<SharedString>,
        progress: model::Progress,
    ) -> Self {
        Self {
            product: product.into(),
            logo,
            progress,
        }
    }
}

impl RenderOnce for OperationProgress {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let progress = self.progress;
        let percent = progress
            .fraction
            .map(|fraction| format!("{}%", (fraction * 100.).floor() as u32));
        let bar = Progress::new("operation-progress")
            .loading(progress.fraction.is_none())
            .value(progress.fraction.unwrap_or(0.) * 100.)
            .accessibility_label(match &percent {
                Some(percent) => format!("{}, {percent}", progress.phase),
                None => progress.phase.clone(),
            })
            .when(progress.stopping, |bar| bar.color(theme.muted_foreground));
        v_flex()
            .items_center()
            .gap(space::XL)
            .child(AppMark::new(self.product, self.logo).size(size::MARK_FOCUS))
            .child(title(progress.title.clone(), cx).text_center())
            .child(
                v_flex()
                    .w_full()
                    .gap(space::SM)
                    .child(
                        h_flex()
                            .justify_between()
                            .gap(space::MD)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(text::BODY)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .child(progress.phase.clone()),
                            )
                            .children(percent.map(|percent| {
                                div()
                                    .flex_shrink_0()
                                    .text_size(text::BODY)
                                    .text_color(theme.muted_foreground)
                                    .child(percent)
                            })),
                    )
                    .child(bar)
                    .child(
                        h_flex()
                            .justify_between()
                            .gap(space::MD)
                            .min_h(text::BODY * 1.5)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .text_size(text::SMALL)
                                    .text_color(theme.muted_foreground)
                                    .children(progress.activity),
                            )
                            .children(progress.amount.map(|amount| {
                                div()
                                    .flex_shrink_0()
                                    .text_size(text::SMALL)
                                    .text_color(theme.muted_foreground)
                                    .child(amount)
                            })),
                    ),
            )
    }
}

/// A committed operation: what is true now, and what to do next.
#[derive(IntoElement)]
pub struct OutcomeView {
    product: SharedString,
    logo: Option<SharedString>,
    outcome: model::Outcome,
}

impl OutcomeView {
    pub fn new(
        product: impl Into<SharedString>,
        logo: Option<SharedString>,
        outcome: model::Outcome,
    ) -> Self {
        Self {
            product: product.into(),
            logo,
            outcome,
        }
    }
}

impl RenderOnce for OutcomeView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let outcome = self.outcome;
        let left_alone = outcome.left_alone.clone();
        v_flex()
            .items_center()
            .gap(space::XL)
            .child(
                AppMark::new(self.product, self.logo)
                    .size(size::MARK_FOCUS)
                    .faded(outcome.removed)
                    .badge((!outcome.removed).then_some(MarkBadge::Done)),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap(space::SM)
                    .child(title(outcome.title.clone(), cx).text_center())
                    .child(muted(outcome.detail.clone(), cx).text_center()),
            )
            .when_some(outcome.note.clone(), |this, note| {
                if left_alone.is_empty() {
                    this.child(
                        div()
                            .max_w(size::FOCUS_COLUMN)
                            .child(caption(note, cx).text_center()),
                    )
                } else {
                    this.child(
                        div().w_full().child(
                            Callout::new(Tone::Attention, IconName::Info)
                                .title("Some files were left as they are")
                                .body(note)
                                .child(v_flex().gap(space::XS).pt(space::XS).children(
                                    left_alone.iter().map(|resource| {
                                        div()
                                            .text_size(text::SMALL)
                                            .line_height(relative(1.4))
                                            .text_color(theme.foreground.opacity(0.85))
                                            .child(resource.clone())
                                    }),
                                )),
                        ),
                    )
                }
            })
    }
}

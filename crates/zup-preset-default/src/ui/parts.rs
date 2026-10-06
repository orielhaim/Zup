//! Type, sections, callouts and the action bar.

// GPUI's derive macros emit paths rooted at gpui_kit rather than at the crate
// that re-exports it, so a module deriving one has to be able to name that
// crate. Aliased through the SDK so it is the version this preset builds with.
#[allow(unused_imports)]
use zup_preset_sdk::gpui_kit;

use zup_preset_sdk::gpui_kit::assets::IconName;
use zup_preset_sdk::gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use zup_preset_sdk::gpui_kit::prelude::FluentBuilder as _;
use zup_preset_sdk::gpui_kit::{
    AnyElement, App, Div, FontWeight, Hsla, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, Window, div, relative,
};

use crate::model::{Fact, FactKind};
use crate::theme::{Layout, size, space, text};

pub use crate::model::Tone;

/// The one line a screen is about.
pub fn title(content: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(text::TITLE)
        .font_weight(FontWeight::SEMIBOLD)
        .line_height(relative(1.2))
        .text_color(cx.theme().foreground)
        .child(content.into())
}

/// Text that explains rather than names.
pub fn muted(content: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(text::BODY)
        .line_height(relative(1.45))
        .text_color(cx.theme().muted_foreground)
        .child(content.into())
}

/// Fine print.
pub fn caption(content: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(text::CAPTION)
        .line_height(relative(1.4))
        .text_color(cx.theme().muted_foreground)
        .child(content.into())
}

/// The colour a tone draws its icon and emphasis in.
pub fn tone_color(tone: Tone, cx: &App) -> Hsla {
    let theme = cx.theme();
    match tone {
        Tone::Neutral => theme.muted_foreground,
        Tone::Positive => theme.success,
        Tone::Attention => theme.warning,
        Tone::Negative => theme.danger,
    }
}

/// A labelled group of related things, separated by space rather than a box.
#[derive(IntoElement)]
pub struct Section {
    label: SharedString,
    trailing: Option<AnyElement>,
    children: Vec<AnyElement>,
}

impl Section {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            trailing: None,
            children: Vec::new(),
        }
    }

    /// Something beside the label, such as a count.
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }
}

impl ParentElement for Section {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Section {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        v_flex()
            .gap(space::SM)
            .child(
                h_flex()
                    .px(space::MD)
                    .justify_between()
                    .gap(space::MD)
                    .child(
                        div()
                            .text_size(text::SMALL)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child(self.label),
                    )
                    .children(self.trailing),
            )
            .children(self.children)
    }
}

/// A message that belongs beside the content, tinted by how much it matters.
#[derive(IntoElement)]
pub struct Callout {
    tone: Tone,
    icon: IconName,
    title: Option<SharedString>,
    body: Option<SharedString>,
    children: Vec<AnyElement>,
    action: Option<AnyElement>,
}

impl Callout {
    pub fn new(tone: Tone, icon: IconName) -> Self {
        Self {
            tone,
            icon,
            title: None,
            body: None,
            children: Vec::new(),
            action: None,
        }
    }

    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn body(mut self, body: impl Into<SharedString>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// The control that acts on what the callout says.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }
}

impl ParentElement for Callout {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Callout {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let accent = tone_color(self.tone, cx);
        let (background, border) = match self.tone {
            Tone::Neutral => (theme.muted.opacity(0.5), theme.border.opacity(0.6)),
            _ => (
                accent.opacity(if theme.is_dark() { 0.1 } else { 0.06 }),
                accent.opacity(0.28),
            ),
        };
        h_flex()
            .items_start()
            .gap(space::MD)
            .px(space::LG)
            .py(space::MD)
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(border)
            .bg(background)
            .child(
                div()
                    .pt(space::HAIR)
                    .child(Icon::new(self.icon).size(size::ICON).text_color(accent)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(space::XS)
                    .children(self.title.map(|title| {
                        div()
                            .text_size(text::BODY)
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.4))
                            .text_color(theme.foreground)
                            .child(title)
                    }))
                    .children(self.body.map(|body| muted(body, cx)))
                    .children(self.children)
                    .children(self.action.map(|action| div().pt(space::SM).child(action))),
            )
    }
}

/// The facts a person should know before installing, in one wrapping line.
#[derive(IntoElement)]
pub struct InstallSummary {
    facts: Vec<Fact>,
}

impl InstallSummary {
    pub fn new(facts: Vec<Fact>) -> Self {
        Self { facts }
    }
}

impl RenderOnce for InstallSummary {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_wrap()
            .gap_x(space::LG)
            .gap_y(space::XS)
            .text_size(text::SMALL)
            .children(self.facts.into_iter().map(|fact| {
                let (icon, color, emphasis) = match fact.kind {
                    FactKind::Size => (IconName::HardDrive, theme.muted_foreground, false),
                    FactKind::Download => (IconName::Download, theme.muted_foreground, false),
                    FactKind::Scope(zup_preset_sdk::prelude::InstallScope::User) => {
                        (IconName::User, theme.muted_foreground, false)
                    }
                    FactKind::Scope(zup_preset_sdk::prelude::InstallScope::Machine) => {
                        (IconName::Users, theme.muted_foreground, false)
                    }
                    FactKind::Approval { required: true } => {
                        (IconName::ShieldAlert, theme.warning, true)
                    }
                    FactKind::Approval { required: false } => {
                        (IconName::ShieldCheck, theme.muted_foreground, false)
                    }
                };
                h_flex()
                    .gap(space::XS)
                    .flex_shrink_0()
                    .child(Icon::new(icon).size(size::ICON_SM).text_color(color))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_color(if emphasis {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .when(emphasis, |this| this.font_weight(FontWeight::MEDIUM))
                            .child(fact.text),
                    )
            }))
    }
}

/// The bar at the bottom of the window that holds what a screen can do.
///
/// The main action is always last and on the right, where the eye ends. When
/// the window is narrow, the leading content moves above the buttons rather
/// than squeezing them.
#[derive(IntoElement)]
pub struct ActionBar {
    layout: Layout,
    leading: Vec<AnyElement>,
    trailing: Vec<AnyElement>,
}

impl ActionBar {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            leading: Vec::new(),
            trailing: Vec::new(),
        }
    }

    pub fn leading(mut self, element: impl IntoElement) -> Self {
        self.leading.push(element.into_any_element());
        self
    }

    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing.push(element.into_any_element());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.leading.is_empty() && self.trailing.is_empty()
    }
}

impl RenderOnce for ActionBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let compact = self.layout.is_compact();
        let leading = h_flex()
            .flex_wrap()
            .items_center()
            .gap_x(space::LG)
            .gap_y(space::XS)
            .children(self.leading);
        let trailing = h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_end()
            .gap(space::SM)
            .children(self.trailing);
        div()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border.opacity(0.7))
            .bg(theme.background)
            .px(self.layout.gutter())
            .py(space::MD)
            .child(if compact {
                v_flex().w_full().gap(space::MD).child(leading).child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .child(div().flex_1())
                        .child(trailing),
                )
            } else {
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(space::LG)
                    .child(leading.flex_1().min_w_0())
                    .child(trailing)
            })
    }
}

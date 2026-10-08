#[allow(unused_imports)]
use zup_sdk::preset::gpui_kit;

use zup_sdk::preset::gpui_kit::assets::IconName;
use zup_sdk::preset::gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use zup_sdk::preset::gpui_kit::prelude::FluentBuilder as _;
use zup_sdk::preset::gpui_kit::{
    App, FontWeight, IntoElement, ObjectFit, ParentElement, Rems, RenderOnce, SharedString, Styled,
    StyledImage, Window, div, img, relative,
};
use zup_sdk::preset::prelude::ProductIdentity;

use crate::theme::{size, space, text};
use crate::ui::{caption, muted};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkBadge {
    Done,
}

#[derive(IntoElement)]
pub struct AppMark {
    name: SharedString,
    logo: Option<SharedString>,
    size: Rems,
    badge: Option<MarkBadge>,
    faded: bool,
}

impl AppMark {
    pub fn new(name: impl Into<SharedString>, logo: Option<SharedString>) -> Self {
        Self {
            name: name.into(),
            logo,
            size: size::MARK,
            badge: None,
            faded: false,
        }
    }

    pub fn size(mut self, size: Rems) -> Self {
        self.size = size;
        self
    }

    pub fn badge(mut self, badge: Option<MarkBadge>) -> Self {
        self.badge = badge;
        self
    }

    pub fn faded(mut self, faded: bool) -> Self {
        self.faded = faded;
        self
    }
}

pub fn initials(name: &str) -> String {
    let mut words = name
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|word| word.chars().next().is_some_and(char::is_alphanumeric));
    let first = words.next().and_then(|word| word.chars().next());
    let second = words.next().and_then(|word| word.chars().next());
    first
        .into_iter()
        .chain(second)
        .flat_map(char::to_uppercase)
        .collect()
}

impl RenderOnce for AppMark {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let size = self.size;
        let radius = size * 0.24;
        let monogram = {
            let letters = initials(&self.name);
            let accent = theme.primary;
            let tint = if theme.is_dark() { 0.22 } else { 0.12 };
            move || {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(radius)
                    .bg(accent.opacity(tint))
                    .border_1()
                    .border_color(accent.opacity(0.18))
                    .text_color(accent)
                    .text_size(size * 0.4)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(letters.clone())
                    .into_any_element()
            }
        };
        let mark = match self.logo {
            Some(logo) => img(logo)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(monogram)
                .into_any_element(),
            None => monogram(),
        };
        div()
            .relative()
            .flex_shrink_0()
            .size(size)
            .when(self.faded, |this| this.opacity(0.45))
            .child(mark)
            .when_some(self.badge, |this, MarkBadge::Done| {
                let badge = size * 0.36;
                this.child(
                    div()
                        .absolute()
                        .right(-(size * 0.06))
                        .bottom(-(size * 0.06))
                        .size(badge)
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(theme.success)
                        .border_2()
                        .border_color(theme.background)
                        .child(
                            Icon::new(IconName::Check)
                                .size(badge * 0.6)
                                .text_color(theme.success_foreground),
                        ),
                )
            })
    }
}

#[derive(IntoElement)]
pub struct AppIdentity {
    product: ProductIdentity,
    logo: Option<SharedString>,
    byline: SharedString,
    note: Option<SharedString>,
    compact: bool,
}

impl AppIdentity {
    pub fn new(product: &ProductIdentity, logo: Option<SharedString>) -> Self {
        Self {
            byline: crate::model::byline(product).into(),
            product: product.clone(),
            logo,
            note: None,
            compact: false,
        }
    }

    pub fn byline(mut self, byline: impl Into<SharedString>) -> Self {
        self.byline = byline.into();
        self
    }

    pub fn note(mut self, note: Option<String>) -> Self {
        self.note = note.map(Into::into);
        self
    }

    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }
}

impl RenderOnce for AppIdentity {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let mark = if self.compact { Rems(2.75) } else { size::MARK };
        h_flex()
            .items_start()
            .gap(space::LG)
            .child(AppMark::new(self.product.name.clone(), self.logo).size(mark))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(space::XS)
                    .child(
                        div()
                            .text_size(if self.compact {
                                text::HEADING
                            } else {
                                text::TITLE
                            })
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.2))
                            .text_color(theme.foreground)
                            .child(self.product.name.clone()),
                    )
                    .child(muted(self.byline, cx))
                    .children(
                        self.product
                            .description
                            .clone()
                            .filter(|_| !self.compact)
                            .map(|description| muted(description, cx).line_clamp(2)),
                    )
                    .children(self.note.map(|note| caption(note, cx))),
            )
    }
}

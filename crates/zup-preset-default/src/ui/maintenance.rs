#[allow(unused_imports)]
use zup_sdk::preset::gpui_kit;

use zup_sdk::preset::gpui_kit::assets::IconName;
use zup_sdk::preset::gpui_kit::component::button::{Button, ButtonVariants};
use zup_sdk::preset::gpui_kit::component::spinner::Spinner;
use zup_sdk::preset::gpui_kit::component::{ActiveTheme, Icon, Sizable, h_flex, v_flex};
use zup_sdk::preset::gpui_kit::prelude::FluentBuilder as _;
use zup_sdk::preset::gpui_kit::{
    AnyElement, App, ElementId, FontWeight, Hsla, IntoElement, ParentElement, RenderOnce,
    SharedString, Styled, Window, div, relative,
};

use crate::model;
use crate::theme::{Layout, size, space, text};
use crate::ui::parts::tone_color;
use crate::ui::{Callout, Handler, Tone, caption};

fn tile(icon: IconName, color: Hsla, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .size(size::ICON_TILE)
        .rounded(theme.radius)
        .flex()
        .items_center()
        .justify_center()
        .bg(color.opacity(if theme.is_dark() { 0.16 } else { 0.1 }))
        .child(Icon::new(icon).size(size::ICON).text_color(color))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Normal,
    Primary,
    Destructive,
}

#[derive(IntoElement)]
pub struct ActionRow {
    id: SharedString,
    icon: IconName,
    tone: Tone,
    title: SharedString,
    detail: SharedString,
    button: Option<(SharedString, Emphasis)>,
    busy: bool,
    layout: Layout,
    on_press: Option<Handler>,
}

impl ActionRow {
    pub fn new(
        id: impl Into<SharedString>,
        icon: IconName,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon,
            tone: Tone::Neutral,
            title: title.into(),
            detail: detail.into(),
            button: None,
            busy: false,
            layout: Layout::Regular,
            on_press: None,
        }
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    pub fn button(
        mut self,
        label: impl Into<SharedString>,
        emphasis: Emphasis,
        on_press: Handler,
    ) -> Self {
        self.button = Some((label.into(), emphasis));
        self.on_press = Some(on_press);
        self
    }

    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }

    pub fn layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }
}

impl RenderOnce for ActionRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let color = match self.tone {
            Tone::Neutral => theme.foreground.opacity(0.75),
            tone => tone_color(tone, cx),
        };
        let button = self
            .button
            .zip(self.on_press)
            .map(|((label, emphasis), press)| {
                let button = Button::new(ElementId::Name(format!("{}-action", self.id).into()))
                    .label(label.clone())
                    .accessibility_label(format!("{label}: {}", self.title))
                    .on_click(move |_, window, cx| press(window, cx));
                match emphasis {
                    Emphasis::Normal => button,
                    Emphasis::Primary => button.primary(),
                    Emphasis::Destructive => button.danger().outline(),
                }
            });
        let text_block = v_flex()
            .flex_1()
            .min_w_0()
            .gap(space::HAIR)
            .child(
                h_flex()
                    .gap(space::SM)
                    .child(
                        div()
                            .text_size(text::BODY)
                            .font_weight(FontWeight::MEDIUM)
                            .line_height(relative(1.35))
                            .text_color(theme.foreground)
                            .child(self.title),
                    )
                    .when(self.busy, |this| {
                        this.child(Spinner::new().small().color(theme.muted_foreground))
                    }),
            )
            .child(caption(self.detail, cx).text_size(text::SMALL));
        let compact = self.layout.is_compact();
        h_flex()
            .items_start()
            .gap(space::MD)
            .py(space::MD)
            .child(tile(self.icon, color, cx))
            .child(if compact {
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(space::SM)
                    .child(text_block)
                    .children(button.map(|button| div().child(button)))
                    .into_any_element()
            } else {
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(space::LG)
                    .child(text_block)
                    .children(button.map(|button| div().flex_shrink_0().child(button)))
                    .into_any_element()
            })
    }
}

#[derive(IntoElement)]
pub struct UpdateRow {
    row: model::UpdateRow,
    layout: Layout,
    on_update: Handler,
}

impl UpdateRow {
    pub fn new(row: model::UpdateRow, layout: Layout, on_update: Handler) -> Self {
        Self {
            row,
            layout,
            on_update,
        }
    }
}

impl RenderOnce for UpdateRow {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let row = self.row;
        let icon = match row.tone {
            Tone::Positive => IconName::BadgeCheck,
            Tone::Attention => IconName::CircleArrowUp,
            Tone::Negative => IconName::CircleAlert,
            Tone::Neutral => IconName::RefreshCw,
        };
        let mut action = ActionRow::new("updates", icon, row.title, row.detail)
            .tone(row.tone)
            .busy(row.busy)
            .layout(self.layout);
        if let Some((label, primary)) = row.action {
            let emphasis = if primary {
                Emphasis::Primary
            } else {
                Emphasis::Normal
            };
            action = action.button(label, emphasis, self.on_update);
        }
        action
    }
}

#[derive(IntoElement)]
pub struct HealthNotice {
    resources: Vec<String>,
    on_repair: Handler,
}

impl HealthNotice {
    pub fn new(resources: Vec<String>, on_repair: Handler) -> Self {
        Self {
            resources,
            on_repair,
        }
    }
}

impl RenderOnce for HealthNotice {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        const SHOWN: usize = 3;
        let theme = cx.theme();
        let count = self.resources.len();
        let repair = self.on_repair.clone();
        Callout::new(Tone::Attention, IconName::TriangleAlert)
            .title(if count == 1 {
                "An installed file was changed".to_owned()
            } else {
                format!("{count} installed files were changed")
            })
            .body(
                "Repair puts back what setup installed. Anything changed on purpose is \
                 kept and listed afterwards.",
            )
            .child(
                v_flex()
                    .gap(space::HAIR)
                    .pt(space::XS)
                    .children(self.resources.iter().take(SHOWN).map(|resource| {
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .text_size(text::SMALL)
                            .text_color(theme.foreground.opacity(0.85))
                            .child(resource.clone())
                    }))
                    .when(count > SHOWN, |this| {
                        this.child(caption(format!("and {} more", count - SHOWN), cx))
                    }),
            )
            .action(
                Button::new("repair-now")
                    .primary()
                    .label("Repair")
                    .on_click(move |_, window, cx| repair(window, cx)),
            )
    }
}

#[derive(IntoElement)]
pub struct DestructiveSection {
    title: SharedString,
    detail: SharedString,
    label: SharedString,
    layout: Layout,
    on_press: Handler,
}

impl DestructiveSection {
    pub fn new(
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        label: impl Into<SharedString>,
        layout: Layout,
        on_press: Handler,
    ) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            label: label.into(),
            layout,
            on_press,
        }
    }
}

impl RenderOnce for DestructiveSection {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let press = self.on_press.clone();
        let children: Vec<AnyElement> = vec![
            ActionRow::new("uninstall", IconName::Trash, self.title, self.detail)
                .tone(Tone::Negative)
                .layout(self.layout)
                .button(self.label, Emphasis::Destructive, press)
                .into_any_element(),
        ];
        v_flex()
            .border_t_1()
            .border_color(theme.border.opacity(0.7))
            .pt(space::SM)
            .children(children)
    }
}

//! The choices an installation offers: where, for whom, and with what.

// GPUI's derive macros emit paths rooted at gpui_kit rather than at the crate
// that re-exports it, so a module deriving one has to be able to name that
// crate. Aliased through the SDK so it is the version this preset builds with.
#[allow(unused_imports)]
use zup_preset_sdk::gpui_kit;

use zup_preset_sdk::gpui_kit::assets::IconName;
use zup_preset_sdk::gpui_kit::component::button::{Button, ButtonVariants};
use zup_preset_sdk::gpui_kit::component::checkbox::Checkbox;
use zup_preset_sdk::gpui_kit::component::collapsible::Collapsible;
use zup_preset_sdk::gpui_kit::component::radio::{Radio, RadioGroup};
use zup_preset_sdk::gpui_kit::component::tag::Tag;
use zup_preset_sdk::gpui_kit::component::tooltip::Tooltip;
use zup_preset_sdk::gpui_kit::component::{ActiveTheme, Icon, Sizable, h_flex, v_flex};
use zup_preset_sdk::gpui_kit::prelude::FluentBuilder as _;
use zup_preset_sdk::gpui_kit::{
    AnyElement, App, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div, radians, relative,
};
use zup_preset_sdk::prelude::InstallScope;

use crate::model::{ComponentKind, ComponentRow, Location, Pending, ScopeChoice as Choice};
use crate::theme::{size, space, text};
use crate::ui::{ChoiceHandler, Handler, caption};

/// Where the installation goes, and the way to put it somewhere else.
///
/// A path, not a text box: the folder is chosen with the system's own folder
/// picker. A long path keeps its beginning and its end, and shows in full on
/// hover.
#[derive(IntoElement)]
pub struct PathChooser {
    label: SharedString,
    location: Location,
    on_change: Option<Handler>,
    on_reset: Option<Handler>,
}

impl PathChooser {
    pub fn new(label: impl Into<SharedString>, location: Location) -> Self {
        Self {
            label: label.into(),
            location,
            on_change: None,
            on_reset: None,
        }
    }

    pub fn on_change(mut self, handler: Handler) -> Self {
        self.on_change = Some(handler);
        self
    }

    pub fn on_reset(mut self, handler: Handler) -> Self {
        self.on_reset = Some(handler);
        self
    }
}

impl RenderOnce for PathChooser {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let path: SharedString = self
            .location
            .path
            .clone()
            .unwrap_or_else(|| "The default location".into())
            .into();
        let known = self.location.path.is_some();
        let changeable = self.location.changeable;
        let tooltip = path.clone();
        v_flex()
            .gap(space::SM)
            .child(
                h_flex()
                    .justify_between()
                    .gap(space::MD)
                    .child(
                        div()
                            .text_size(text::SMALL)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(self.label),
                    )
                    .when(self.location.custom && changeable, |this| {
                        this.children(self.on_reset.clone().map(|reset| {
                            Button::new("location-reset")
                                .link()
                                .small()
                                .label("Use default")
                                .on_click(move |_, window, cx| reset(window, cx))
                        }))
                    }),
            )
            .child(
                h_flex()
                    .gap(space::MD)
                    .pl(space::MD)
                    .pr(if changeable { space::XS } else { space::MD })
                    .py(space::XS)
                    .min_h(rems_of(2.5))
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme
                        .muted
                        .opacity(if theme.is_dark() { 0.35 } else { 0.4 }))
                    .child(
                        Icon::new(IconName::Folder)
                            .size(size::ICON)
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .id("location-path")
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .text_size(text::BODY)
                            .text_color(if known {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .child(path)
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            }),
                    )
                    .children(self.on_change.filter(|_| changeable).map(|change| {
                        Button::new("location-change")
                            .small()
                            .label("Change…")
                            .accessibility_label("Change install location")
                            .on_click(move |_, window, cx| change(window, cx))
                    })),
            )
    }
}

fn rems_of(value: f32) -> zup_preset_sdk::gpui_kit::Rems {
    zup_preset_sdk::gpui_kit::Rems(value)
}

/// Who the installation is for.
#[derive(IntoElement)]
pub struct ScopeChoice {
    choices: Vec<Choice>,
    selected: InstallScope,
    on_select: ChoiceHandler<InstallScope>,
}

impl ScopeChoice {
    pub fn new(
        choices: Vec<Choice>,
        selected: InstallScope,
        on_select: impl Fn(InstallScope, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            choices,
            selected,
            on_select: std::rc::Rc::new(on_select),
        }
    }
}

impl RenderOnce for ScopeChoice {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let selected = self
            .choices
            .iter()
            .position(|choice| choice.scope == self.selected);
        let scopes: Vec<InstallScope> = self.choices.iter().map(|choice| choice.scope).collect();
        let on_select = self.on_select.clone();
        let warning = theme.warning;
        let muted_color = theme.muted_foreground;
        RadioGroup::vertical("install-scope")
            .selected_index(selected)
            .on_change(move |index, window, cx| {
                if let Some(scope) = scopes.get(*index) {
                    on_select(*scope, window, cx);
                }
            })
            .children(self.choices.into_iter().map(|choice| {
                Radio::new(SharedString::from(choice.title))
                    .label(choice.title)
                    .accessibility_label(format!("{}. {}", choice.title, choice.detail))
                    .child(
                        h_flex()
                            .gap(space::XS)
                            .items_start()
                            .when(choice.needs_approval, |this| {
                                this.child(
                                    div().pt(space::HAIR).child(
                                        Icon::new(IconName::ShieldAlert)
                                            .size(size::ICON_SM)
                                            .text_color(warning),
                                    ),
                                )
                            })
                            .child(
                                div()
                                    .text_size(text::SMALL)
                                    .line_height(relative(1.4))
                                    .text_color(muted_color)
                                    .child(choice.detail),
                            ),
                    )
            }))
    }
}

/// One component, as a choice or as something always included.
#[derive(IntoElement)]
pub struct ComponentChoice {
    row: ComponentRow,
    on_toggle: Handler,
}

impl ComponentChoice {
    pub fn new(row: ComponentRow, on_toggle: Handler) -> Self {
        Self { row, on_toggle }
    }
}

impl RenderOnce for ComponentChoice {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let name = self.row.name.clone();
        let pending = self.row.pending.map(|pending| match pending {
            Pending::Add => Tag::success().outline().xsmall().child("Will be added"),
            Pending::Remove => Tag::danger().outline().xsmall().child("Will be removed"),
        });
        let heading = h_flex()
            .flex_wrap()
            .gap_x(space::SM)
            .gap_y(space::XS)
            .child(
                div()
                    .text_size(text::BODY)
                    .font_weight(FontWeight::MEDIUM)
                    .line_height(relative(1.35))
                    .text_color(theme.foreground)
                    .child(name.clone()),
            )
            .when(self.row.kind == ComponentKind::Required, |this| {
                this.child(
                    div()
                        .text_size(text::SMALL)
                        .line_height(relative(1.35))
                        .text_color(theme.muted_foreground)
                        .child("Required"),
                )
            })
            .children(pending);
        let description = self
            .row
            .description
            .clone()
            .map(|description| caption(description, cx).text_size(text::SMALL));
        let body = v_flex()
            .gap(space::HAIR)
            .child(heading)
            .children(description);

        let row = div()
            .w_full()
            .px(space::MD)
            .py(space::XS)
            .rounded(theme.radius)
            .hover(|style| style.bg(theme.muted.opacity(0.45)));
        match self.row.kind {
            ComponentKind::Required => row
                .child(
                    h_flex()
                        .items_center()
                        .gap(space::SM)
                        .child(
                            Icon::new(IconName::CircleCheck)
                                .size(size::ICON)
                                .text_color(theme.muted_foreground),
                        )
                        .child(body.flex_1().min_w_0()),
                )
                .into_any_element(),
            ComponentKind::Optional { selected } => {
                let toggle = self.on_toggle.clone();
                row.child(
                    Checkbox::new(ElementId::Name(format!("component-{}", self.row.id).into()))
                        .checked(selected)
                        .accessibility_label(name)
                        .child(body)
                        .on_change(move |_, window, cx| toggle(window, cx)),
                )
                .into_any_element()
            }
        }
    }
}

/// A large primary group: the decision is visible, and choosing opens the whole set.
#[derive(IntoElement)]
pub struct GroupSummary {
    title: SharedString,
    count: SharedString,
    names: SharedString,
    on_open: Handler,
}

impl GroupSummary {
    pub fn new(
        title: impl Into<SharedString>,
        count: impl Into<SharedString>,
        names: impl Into<SharedString>,
        on_open: Handler,
    ) -> Self {
        Self {
            title: title.into(),
            count: count.into(),
            names: names.into(),
            on_open,
        }
    }
}

impl RenderOnce for GroupSummary {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let open = self.on_open.clone();
        v_flex()
            .gap(space::SM)
            .child(
                h_flex()
                    .justify_between()
                    .gap(space::MD)
                    .flex_wrap()
                    .child(
                        div()
                            .text_size(text::SMALL)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(self.title),
                    )
                    .child(
                        div()
                            .text_size(text::SMALL)
                            .text_color(theme.muted_foreground)
                            .child(self.count),
                    ),
            )
            .child(
                div()
                    .text_size(text::BODY)
                    .line_height(relative(1.45))
                    .text_color(theme.foreground)
                    .child(self.names),
            )
            .child(
                Button::new("choose-components")
                    .link()
                    .label("Choose components…")
                    .on_click(move |_, window, cx| open(window, cx)),
            )
    }
}

/// A heading that shows or hides what is under it.
#[derive(IntoElement)]
pub struct Disclosure {
    id: SharedString,
    title: SharedString,
    summary: Option<SharedString>,
    open: bool,
    on_toggle: Handler,
    content: Vec<AnyElement>,
}

impl Disclosure {
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        open: bool,
        on_toggle: Handler,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            summary: None,
            open,
            on_toggle,
            content: Vec::new(),
        }
    }

    pub fn summary(mut self, summary: Option<String>) -> Self {
        self.summary = summary.map(Into::into);
        self
    }
}

impl ParentElement for Disclosure {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.content.extend(elements);
    }
}

impl RenderOnce for Disclosure {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let open = self.open;
        let toggle = self.on_toggle.clone();
        // The chevron points along the line and turns when the row opens.
        // A layout direction that mirrors the interface should mirror it too;
        // the rotation itself is the open state, not a "next page" affordance.
        let trigger = div()
            .id(ElementId::Name(format!("{}-trigger", self.id).into()))
            .w_full()
            .px(space::MD)
            .py(space::SM)
            .rounded(theme.radius)
            .hover(|style| style.bg(theme.muted.opacity(0.35)))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .flex_wrap()
                    .gap_x(space::SM)
                    .gap_y(space::XS)
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size(size::ICON_SM)
                            .text_color(theme.muted_foreground)
                            .when(open, |icon| {
                                icon.rotate(radians(std::f32::consts::FRAC_PI_2))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(text::BODY)
                            .font_weight(FontWeight::MEDIUM)
                            .line_height(relative(1.4))
                            .text_color(theme.foreground)
                            .child(self.title),
                    )
                    .children(self.summary.filter(|_| !open).map(|summary| {
                        div()
                            .text_size(text::SMALL)
                            .line_height(relative(1.4))
                            .text_color(theme.muted_foreground)
                            .child(summary)
                    })),
            )
            .on_click(move |_, window, cx| toggle(window, cx));
        Collapsible::new()
            .motion_id(ElementId::Name(self.id.clone()))
            .open(open)
            .gap(space::MD)
            .child(trigger)
            .content(
                v_flex()
                    .gap(space::LG)
                    .pt(space::SM)
                    .pb(space::XS)
                    .children(self.content),
            )
    }
}

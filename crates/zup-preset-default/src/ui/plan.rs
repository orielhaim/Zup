//! What the current choices would change, in full.
//!
//! The cost and the blast radius first, then every change grouped by what kind
//! of thing it touches. It is for a person deciding whether to go ahead, so it
//! reads as a list of consequences rather than a transaction log.

// GPUI's derive macros emit paths rooted at gpui_kit rather than at the crate
// that re-exports it, so a module deriving one has to be able to name that
// crate. Aliased through the SDK so it is the version this preset builds with.
#[allow(unused_imports)]
use zup_preset_sdk::gpui_kit;

use std::collections::BTreeSet;
use std::rc::Rc;

use zup_preset_sdk::gpui_kit::assets::IconName;
use zup_preset_sdk::gpui_kit::component::spinner::Spinner;
use zup_preset_sdk::gpui_kit::component::tag::Tag;
use zup_preset_sdk::gpui_kit::component::{ActiveTheme, Icon, Sizable, h_flex, v_flex};
use zup_preset_sdk::gpui_kit::prelude::FluentBuilder as _;
use zup_preset_sdk::gpui_kit::{
    AnyElement, App, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, StatefulInteractiveElement, Styled, Window, div, radians, relative,
};
use zup_preset_sdk::host::{
    ChangeGroup, ChangeKind, PlannedChange, RequirementPresentation, RequirementStatus,
    ResourceCategory,
};
use zup_preset_sdk::prelude::*;

use crate::model;
use crate::theme::{size, space, text};
use crate::ui::{Callout, ChoiceHandler, Handler, Section, Tone, caption, muted};

/// The icon a resource category is marked with.
fn category_icon(category: ResourceCategory) -> IconName {
    match category {
        ResourceCategory::Files => IconName::Files,
        ResourceCategory::Launchers => IconName::AppWindow,
        ResourceCategory::Path => IconName::SquareTerminal,
        ResourceCategory::Services => IconName::Server,
        ResourceCategory::Protocols => IconName::Link,
        ResourceCategory::FileAssociations => IconName::FileText,
        ResourceCategory::AppsFeatures => IconName::ListChecks,
        ResourceCategory::Maintenance => IconName::Wrench,
        ResourceCategory::Prerequisites => IconName::Puzzle,
        ResourceCategory::Other => IconName::Layers,
    }
}

/// A change's verb as a small coloured tag; the word carries the meaning, the
/// colour only repeats it.
fn change_tag(kind: ChangeKind) -> Tag {
    let label = model::change_label(kind);
    match kind {
        ChangeKind::Create => Tag::success().outline().xsmall().child(label),
        ChangeKind::Update => Tag::info().outline().xsmall().child(label),
        ChangeKind::Remove => Tag::danger().outline().xsmall().child(label),
        ChangeKind::Drifted | ChangeKind::Conflict => {
            Tag::warning().outline().xsmall().child(label)
        }
        ChangeKind::NoChange => Tag::secondary().xsmall().child(label),
    }
}

/// The content of the "What will change" sheet.
#[derive(IntoElement)]
pub struct PlanDetails {
    plan: PlanStatus,
    context: (String, Option<String>),
    expanded: BTreeSet<ResourceCategory>,
    on_toggle: ChoiceHandler<ResourceCategory>,
}

impl PlanDetails {
    pub fn new(
        plan: PlanStatus,
        context: (String, Option<String>),
        expanded: BTreeSet<ResourceCategory>,
        on_toggle: impl Fn(ResourceCategory, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            plan,
            context,
            expanded,
            on_toggle: Rc::new(on_toggle),
        }
    }
}

impl RenderOnce for PlanDetails {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let computing = self.plan.is_computing();
        let Some(plan) = self.plan.latest().cloned() else {
            return v_flex()
                .gap(space::LG)
                .py(space::LG)
                .child(match &self.plan {
                    PlanStatus::Failed { reason } => Callout::new(Tone::Attention, IconName::Info)
                        .title("The changes couldn't be listed")
                        .body(format!(
                            "You can still continue; setup checks everything again before it \
                             changes anything. ({reason})"
                        ))
                        .into_any_element(),
                    PlanStatus::Unsupported => Callout::new(Tone::Neutral, IconName::Info)
                        .body("This installer doesn't list its changes ahead of time.")
                        .into_any_element(),
                    _ => h_flex()
                        .gap(space::SM)
                        .child(Spinner::new().small())
                        .child(muted("Working out what will change…", cx))
                        .into_any_element(),
                })
                .into_any_element();
        };

        let changing: Vec<&ChangeGroup> = plan
            .groups
            .iter()
            .filter(|group| model::group_changes_anything(group))
            .collect();
        let unchanged: usize = plan
            .groups
            .iter()
            .flat_map(|group| &group.changes)
            .filter(|change| change.kind == ChangeKind::NoChange)
            .count();

        v_flex()
            .gap(space::XL)
            .px(space::MD)
            .pt(space::MD)
            .pb(space::XL)
            .when(computing, |this| {
                this.child(
                    h_flex()
                        .gap(space::SM)
                        .child(Spinner::new().xsmall())
                        .child(caption("Updating for your latest choices…", cx)),
                )
            })
            .child(ContextLine::new(self.context.clone()))
            .when(!plan.requirements.is_empty(), |this| {
                this.child(requirements(&plan.requirements, cx))
            })
            .child(
                Section::new("Changes to this computer")
                    .when(unchanged > 0, |section| {
                        section.trailing(caption(format!("{unchanged} unchanged"), cx))
                    })
                    .when(changing.is_empty(), |section| {
                        section.child(muted("Nothing here would change.", cx))
                    })
                    .child(
                        v_flex()
                            .gap(space::XS)
                            .children(changing.into_iter().map(|group| {
                                let category = group.category;
                                let toggle = self.on_toggle.clone();
                                Group {
                                    group: group.clone(),
                                    open: self.expanded.contains(&category),
                                    on_toggle: Rc::new(move |window, cx| {
                                        toggle(category, window, cx)
                                    }),
                                }
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// The facts the main screen already summarized, as one piece of context.
#[derive(IntoElement)]
struct ContextLine {
    line: String,
    location: Option<String>,
}

impl ContextLine {
    fn new(context: (String, Option<String>)) -> Self {
        Self {
            line: context.0,
            location: context.1,
        }
    }
}

impl RenderOnce for ContextLine {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap(space::XS)
            .when(!self.line.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(text::SMALL)
                        .line_height(relative(1.45))
                        .text_color(theme.muted_foreground)
                        .child(self.line),
                )
            })
            .when_some(
                self.location.filter(|path| !path.is_empty()),
                |this, path| {
                    this.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .text_size(text::SMALL)
                            .line_height(relative(1.45))
                            .text_color(theme.foreground)
                            .child(path),
                    )
                },
            )
    }
}

fn requirements(requirements: &[RequirementPresentation], cx: &App) -> AnyElement {
    let theme = cx.theme();
    Section::new("Required components")
        .child(
            v_flex()
                .gap(space::SM)
                .children(requirements.iter().map(|requirement| {
                    let status = match requirement.status {
                        RequirementStatus::Missing => Tag::info().outline().xsmall(),
                        RequirementStatus::Satisfied => Tag::secondary().xsmall(),
                        RequirementStatus::Unknown => Tag::secondary().outline().xsmall(),
                    }
                    .child(model::requirement_status(requirement.status));
                    h_flex()
                        .gap(space::MD)
                        .child(
                            Icon::new(IconName::Puzzle)
                                .size(size::ICON_SM)
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(text::BODY)
                                        .text_color(theme.foreground)
                                        .child(requirement.name.clone()),
                                )
                                .when(
                                    requirement.status == RequirementStatus::Missing
                                        && requirement.estimated_bytes > 0,
                                    |this| {
                                        this.child(caption(
                                            format!(
                                                "{} download",
                                                model::size(requirement.estimated_bytes)
                                            ),
                                            cx,
                                        ))
                                    },
                                ),
                        )
                        .child(div().flex_shrink_0().child(status))
                })),
        )
        .into_any_element()
}

/// One category of change, collapsed to its counts until it is opened.
#[derive(IntoElement)]
struct Group {
    group: ChangeGroup,
    open: bool,
    on_toggle: Handler,
}

impl RenderOnce for Group {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let category = self.group.category;
        let toggle = self.on_toggle.clone();
        let title = model::category_title(category);
        let counts = model::counts_line(&self.group);
        let open = self.open;
        let header = div()
            .id(ElementId::Name(format!("plan-group-{category:?}").into()))
            .w_full()
            .px(space::MD)
            .py(space::SM)
            .rounded(theme.radius)
            .hover(|style| style.bg(theme.muted.opacity(0.35)))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(space::SM)
                    .child(
                        Icon::new(category_icon(category))
                            .size(size::ICON)
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .items_start()
                            .child(
                                div()
                                    .text_size(text::BODY)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(text::SMALL)
                                    .line_height(relative(1.4))
                                    .text_color(theme.muted_foreground)
                                    .child(counts),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size(size::ICON_SM)
                            .text_color(theme.muted_foreground)
                            .when(open, |icon| {
                                icon.rotate(radians(std::f32::consts::FRAC_PI_2))
                            }),
                    ),
            )
            .on_click(move |_, window, cx| toggle(window, cx));

        let changes: Vec<&PlannedChange> = self
            .group
            .changes
            .iter()
            .filter(|change| change.kind != ChangeKind::NoChange)
            .collect();
        let hidden = changes.len().saturating_sub(size::PLAN_ROWS);
        v_flex().child(header).when(open, |this| {
            this.child(
                v_flex()
                    .ml(space::XXL)
                    .mb(space::SM)
                    .gap(space::XS)
                    .child(caption(model::category_about(category), cx).pb(space::XS))
                    .children(changes.into_iter().take(size::PLAN_ROWS).map(|change| {
                        h_flex()
                            .items_start()
                            .gap(space::SM)
                            .py(space::HAIR)
                            .child(div().flex_shrink_0().child(change_tag(change.kind)))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis_middle()
                                            .text_size(text::SMALL)
                                            .line_height(relative(1.45))
                                            .text_color(theme.foreground)
                                            .child(change.label.clone()),
                                    )
                                    .when_some(
                                        change
                                            .location
                                            .clone()
                                            .filter(|location| location != &change.label),
                                        |this, location| {
                                            this.child(
                                                div()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis_middle()
                                                    .text_size(text::CAPTION)
                                                    .text_color(theme.muted_foreground)
                                                    .child(location),
                                            )
                                        },
                                    ),
                            )
                            .when(change.requires_authorization, |this| {
                                this.child(
                                    Icon::new(IconName::ShieldAlert)
                                        .size(size::ICON_SM)
                                        .text_color(theme.warning.opacity(0.9)),
                                )
                            })
                    }))
                    .when(hidden > 0, |this| {
                        this.child(caption(format!("and {hidden} more"), cx).pt(space::XS))
                    }),
            )
        })
    }
}

/// Whether there is anything worth opening the sheet for.
pub fn has_details(plan: &PlanStatus) -> bool {
    !matches!(plan, PlanStatus::Unsupported)
}

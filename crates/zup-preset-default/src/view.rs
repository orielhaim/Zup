//! The window, and the projection it draws.
//!
//! # Two layers, and why they are separate
//!
//! `controls` is a pure function of one `UiSnapshot`: what a person can turn on,
//! what the window should offer as its main action, and what it should say while
//! the machine is busy. `Page::of` says which page that state belongs to. Both
//! are testable without GPUI, which is the point: a host that publishes the wrong
//! state is a bug in the host, and a window that draws the wrong thing is a bug in
//! the window, and only one of them is visible without a display.
//!
//! Everything below them is layout.
//!
//! # One page per published state, and no more
//!
//! A page is chosen by the state the host publishes. There is no local step, no
//! wizard index and no screen the protocol cannot reach, because a page a real
//! installation cannot be in is a page a preset author designs against and a user
//! never meets - and the first thing to go wrong in a window that invents states
//! is that the preset and the host disagree about where they are.

use std::time::Duration;

use gpui_kit::base::Disableable;
use gpui_kit::base::animation::EffectTransition;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::stepper::{Stepper, StepperItem};
use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, Entity, FontWeight, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, WindowBounds, WindowOptions, div, point, px,
};
use zup_ui_sdk::UiSession;
use zup_ui_sdk::prelude::*;

use crate::Settings;

/// The window's own proportions.
///
/// A window opened at the platform default fills the screen, which makes a short
/// form look like a page that failed to load. An installer is a dialog about one
/// decision at a time, so it is sized like one and stays resizable for the page
/// that needs the room.
const WINDOW_WIDTH: f32 = 880.;
const WINDOW_HEIGHT: f32 = 620.;
/// Where it opens, in pixels from the top left of the work area. A fixed offset
/// rather than a computed centre, because the platform's own dialog placement is
/// better at this than any arithmetic done here would be.
const WINDOW_ORIGIN: (f32, f32) = (96., 72.);
/// The step rail's width, and the air around a page's content.
const RAIL: f32 = 212.;
const PAGE_PADDING: f32 = 32.;

/// How long a page takes to arrive.
const PAGE_TRANSITION: Duration = Duration::from_millis(200);

/// A choice a person can make here, and what making it asks the host for.
pub struct Choice {
    /// Stable across snapshots, so a control keeps its identity when the state
    /// around it changes.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub selected: bool,
    pub required: bool,
    pub action: UiAction,
}

/// Everything the window offers, given the state it was handed.
pub struct Controls {
    pub title: String,
    pub subtitle: String,
    pub scopes: Vec<Choice>,
    pub components: Vec<Choice>,
    /// Whether the install location may be typed into at all.
    pub editable_directory: bool,
    pub directory: Option<String>,
    /// The action the big button performs.
    pub primary: Option<UiAction>,
    pub primary_label: String,
    /// Everything else this state offers, in the order it should be read.
    pub secondary: Vec<(String, UiAction)>,
    /// What the window says instead of, or beneath, the controls.
    pub status: Option<Status>,
}

/// A message about how the installation is going.
pub struct Status {
    pub heading: String,
    pub detail: Option<String>,
    /// 0-100, or `None` while the total is not yet known.
    pub percent: Option<f32>,
    /// Whether a cancel is worth offering.
    pub cancellable: bool,
}

/// Read a snapshot as a window.
pub fn controls(snapshot: &UiSnapshot) -> Controls {
    let mut controls = Controls {
        title: snapshot.product.name.clone(),
        subtitle: match (
            &snapshot.product.publisher,
            snapshot.surface.updates_enabled(),
        ) {
            (Some(publisher), _) => format!("{publisher} · {}", snapshot.product.version),
            (None, _) => snapshot.product.version.clone(),
        },
        scopes: Vec::new(),
        components: Vec::new(),
        editable_directory: snapshot.surface.allows_directory_override(),
        directory: snapshot.surface.install_directory().map(str::to_owned),
        primary: None,
        primary_label: String::new(),
        secondary: Vec::new(),
        status: None,
    };

    if let UiSurface::Install(options) = &snapshot.surface {
        controls.scopes = options
            .scopes
            .iter()
            .map(|scope| Choice {
                id: format!("scope-{scope}"),
                label: scope_label(*scope).to_owned(),
                description: None,
                selected: *scope == options.scope,
                required: true,
                action: UiAction::SetScope { scope: *scope },
            })
            .collect();
        controls.primary_label = if options.existing_version.is_some() {
            "Upgrade".into()
        } else {
            "Install".into()
        };
    }
    controls.components = snapshot
        .surface
        .components()
        .iter()
        .map(|component| Choice {
            id: component.id.to_string(),
            label: component.name.clone(),
            description: component.description.clone(),
            selected: component.selected,
            required: component.required,
            action: UiAction::SetComponent {
                component: component.id.clone(),
                selected: !component.selected,
            },
        })
        .collect();

    match &snapshot.state {
        UiState::Options => {
            controls.primary = Some(UiAction::Install);
            controls
                .secondary
                .push(("Show what will change".into(), UiAction::Preview));
        }
        UiState::Maintenance => {
            controls.primary = Some(UiAction::Modify);
            controls.primary_label = "Change".into();
            if !components_required_only(snapshot) {
                controls.secondary.push(("Repair".into(), UiAction::Repair));
            }
            if snapshot.surface.updates_enabled() {
                controls
                    .secondary
                    .push(("Check for updates".into(), UiAction::Update));
            }
            controls
                .secondary
                .push(("Uninstall".into(), UiAction::RequestUninstall));
        }
        UiState::Running | UiState::WaitingForSafeCancellation => {
            let progress = snapshot.progress.as_ref();
            let (heading, percent) = progress.map_or_else(
                || (String::from("Working…"), None),
                |progress| {
                    (
                        progress.label.clone(),
                        percent(progress.completed, progress.total),
                    )
                },
            );
            let cancellable = snapshot.state == UiState::Running;
            controls.status = Some(Status {
                heading,
                detail: (!cancellable).then(|| String::from("Stopping at the next safe point…")),
                percent,
                cancellable,
            });
            if cancellable {
                controls.secondary.push(("Stop".into(), UiAction::Cancel));
            }
        }
        UiState::Blocked { blockers } => {
            controls.status = Some(Status {
                heading: String::from("Close these and try again"),
                detail: Some(blockers.join("\n")),
                percent: None,
                cancellable: false,
            });
            controls.primary = Some(UiAction::Retry);
            controls.primary_label = String::from("Try again");
        }
        UiState::ConfirmUninstall => {
            controls.primary = Some(UiAction::ConfirmUninstall);
            controls.primary_label = String::from("Uninstall");
            controls
                .secondary
                .push(("Keep it".into(), UiAction::DismissUninstall));
        }
        UiState::Succeeded => {
            controls.status = Some(Status {
                heading: String::from("Done"),
                detail: None,
                percent: Some(100.0),
                cancellable: false,
            });
        }
        UiState::Failed | UiState::RecoveryRequired => {
            controls.status = snapshot.diagnostic.as_ref().map(|diagnostic| Status {
                heading: diagnostic.title.clone(),
                detail: Some(diagnostic.meaning.clone()),
                percent: None,
                cancellable: false,
            });
            controls.primary = Some(UiAction::Retry);
            controls.primary_label = String::from("Try again");
        }
    }

    if !matches!(snapshot.state, UiState::Options | UiState::Maintenance) {
        controls
            .secondary
            .push(("Copy diagnostics".into(), UiAction::CopyDiagnostics));
    }
    if controls.primary.is_none() && !snapshot.state.is_active() {
        controls.primary = Some(UiAction::Close);
        controls.primary_label = String::from("Close");
    }
    controls
}

/// Whether every component on offer is required, which is what makes a repair
/// pointless: there is nothing to restore that a person could have turned off.
fn components_required_only(snapshot: &UiSnapshot) -> bool {
    let components = snapshot.surface.components();
    !components.is_empty() && components.iter().all(|component| component.required)
}

fn percent(completed: u64, total: u64) -> Option<f32> {
    (total > 0).then(|| (completed.min(total) as f32 / total as f32) * 100.0)
}

fn scope_label(scope: InstallScope) -> &'static str {
    match scope {
        InstallScope::User => "Just me",
        InstallScope::Machine => "Everyone on this computer",
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// One page of the window.
///
/// Every variant is a state a real installation can be in, because the page is
/// derived from the published state and from nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Scope, components and location.
    Options,
    /// Work in flight.
    Progress,
    /// Committed.
    Done,
    /// An installation that exists, and what may be done to it.
    Maintenance,
    /// Asked to remove it, waiting to be told to.
    Confirm,
    /// Something is in the way, or something went wrong.
    Problem,
}

impl Page {
    /// The page a published state belongs to.
    pub fn of(state: &UiState) -> Self {
        match state {
            UiState::Options => Self::Options,
            UiState::Maintenance => Self::Maintenance,
            UiState::Running | UiState::WaitingForSafeCancellation => Self::Progress,
            UiState::Succeeded => Self::Done,
            UiState::ConfirmUninstall => Self::Confirm,
            UiState::Blocked { .. } | UiState::Failed | UiState::RecoveryRequired => Self::Problem,
        }
    }

    /// The rail for a machine being installed for the first time, or for one
    /// that already has it.
    ///
    /// Two rails rather than one because the flows are different: an installation
    /// never offers "Uninstall", and a maintenance session never offers a fresh
    /// component choice as its first step.
    pub fn rail(installing: bool) -> &'static [(Self, &'static str)] {
        if installing {
            &[
                (Self::Options, "Options"),
                (Self::Progress, "Installing"),
                (Self::Done, "Finished"),
                (Self::Confirm, "Uninstall"),
                (Self::Problem, "Problems"),
            ]
        } else {
            &[
                (Self::Maintenance, "Installed"),
                (Self::Confirm, "Uninstall"),
                (Self::Progress, "Working"),
                (Self::Done, "Finished"),
                (Self::Problem, "Problems"),
            ]
        }
    }

    /// Where a page sits in a rail, or `None` when the rail does not show it.
    pub fn step(rail: &[(Self, &'static str)], page: Self) -> Option<usize> {
        rail.iter().position(|(known, _)| *known == page)
    }

    /// What the page says above its content.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Options => "Options",
            Self::Progress => "Progress",
            Self::Done => "Finished",
            Self::Maintenance => "Installed",
            Self::Confirm => "Uninstall",
            Self::Problem => "Something went wrong",
        }
    }
}

/// Open the installer window for a session the SDK has already started.
pub fn open(context: PresetContext<Settings>, cx: &mut App) {
    let session = context.session().clone();
    let settings = context.settings().clone();
    let product = session
        .state()
        .read(cx)
        .snapshot()
        .map(|snapshot| snapshot.product.name.clone())
        .unwrap_or_default();

    let state = session.state();
    let _changes = cx.observe(&state, |_, _| {});
    gpui_kit::open_window(window_options(), cx, move |window, cx| {
        window.set_window_title(&format!("{product} Setup"));
        let directory = state
            .read(cx)
            .snapshot()
            .and_then(|snapshot| snapshot.surface.install_directory().map(str::to_owned))
            .unwrap_or_default();
        let location = cx.new(|cx| InputState::new(window, cx).default_value(directory));
        let view = cx.new(|_| Installer {
            session: session.clone(),
            settings: settings.clone(),
            location,
            _settings: Subscription::new(|| {}),
        });
        let refreshed = view.clone();
        view.update(cx, |view, cx| {
            view._settings = settings.observe(cx, move |_, cx| {
                refreshed.update(cx, |_, cx| cx.notify());
            });
        });
        view
    })
    .expect("open the installer window");
}

/// The window's bounds.
fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(WINDOW_ORIGIN.0), px(WINDOW_ORIGIN.1)),
            gpui_kit::size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        ))),
        ..WindowOptions::default()
    }
}

/// The window.
pub struct Installer {
    session: UiSession,
    settings: PresetSettings<Settings>,
    /// The only thing here that is not a rendering of the snapshot: text in a box
    /// has to live somewhere until the host answers with a new one.
    location: Entity<InputState>,
    /// Re-renders the window when the application changes what it configured.
    _settings: Subscription,
}

impl Installer {
    /// The typed location, when the application allows choosing one.
    fn typed_directory(&self, cx: &Context<Self>) -> Option<String> {
        let typed = self.location.read(cx).value().trim().to_owned();
        (!typed.is_empty()).then_some(typed)
    }
}

impl Render for Installer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme: &Theme = cx.theme();
        let Some(snapshot) = self.session.state().read(cx).snapshot().cloned() else {
            return div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .bg(theme.colors.background)
                .text_color(theme.colors.muted_foreground)
                .text_size(px(13.))
                .child(match self.session.state().read(cx).closed() {
                    Some(reason) => format!("Setup closed: {reason}"),
                    None => String::from("Waiting for the installer…"),
                });
        };
        let controls = controls(&snapshot);
        let hero = self.settings.read(cx).hero.clone();
        let installing = matches!(snapshot.surface, UiSurface::Install(_));
        let page = Page::of(&snapshot.state);
        let rail = Page::rail(installing);
        let step = Page::step(rail, page).unwrap_or(0);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.colors.background)
            .text_color(theme.colors.foreground)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .child(self.rail(&controls, hero, rail, step, installing, theme))
                    .child(self.page(&snapshot, &controls, page, theme)),
            )
            .child(self.footer(&controls, theme, cx))
            .overflow_hidden()
    }
}

impl Installer {
    /// The step rail, and the application's identity above it.
    ///
    /// A step indicator and not a control. The flow is the host's, and a rail a
    /// person could click would promise they can step past something they cannot.
    fn rail(
        &self,
        controls: &Controls,
        hero: Option<String>,
        rail: &'static [(Page, &'static str)],
        step: usize,
        installing: bool,
        theme: &Theme,
    ) -> impl IntoElement {
        let items = rail.iter().enumerate().map(|(index, (_, label))| {
            let current = index == step;
            StepperItem::new().child(
                div()
                    .text_size(px(13.))
                    .font_weight(if current {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if current {
                        theme.colors.sidebar_foreground
                    } else {
                        theme.colors.muted_foreground
                    })
                    .child((*label).to_owned()),
            )
        });

        div()
            .w(px(RAIL))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_6()
            .px_5()
            .py_6()
            .bg(theme.colors.sidebar)
            .border_r_1()
            .border_color(theme.colors.sidebar_border)
            .child({
                // Built rather than conditionally appended: there is no
                // `when_some` in this toolkit, and a chain that ends in two
                // optional children is harder to read than the branch it replaces.
                let mut brand = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.colors.sidebar_foreground)
                            .child(controls.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.colors.muted_foreground)
                            .child(controls.subtitle.clone()),
                    );
                if let Some(hero) = hero {
                    brand = brand.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.colors.muted_foreground)
                            .child(hero),
                    );
                }
                brand
            })
            .child(
                Stepper::new("flow")
                    .vertical()
                    .disabled(true)
                    .selected_index(step)
                    .items(items)
                    .into_any_element(),
            )
            .child(
                // The version is in the rail because it is the one fact that has
                // to be true on every page, including the one a person reaches
                // after an error.
                div().mt_auto().flex().flex_col().gap_1().child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.colors.muted_foreground)
                        .child(if installing {
                            "Not installed yet"
                        } else {
                            "Already on this machine"
                        }),
                ),
            )
    }

    /// One page: a heading, its content, and what the session has to add.
    fn page(
        &mut self,
        snapshot: &UiSnapshot,
        controls: &Controls,
        page: Page,
        theme: &Theme,
    ) -> impl IntoElement {
        let subtitle = page_subtitle(page, controls);
        let body: gpui_kit::Div = match page {
            Page::Options => self.options(controls, theme),
            Page::Progress => self.progress(snapshot, controls, theme),
            Page::Done => self.done(controls, theme),
            Page::Maintenance => self.maintenance(controls, theme),
            Page::Confirm => self.confirm(theme),
            Page::Problem => self.problem(snapshot, controls, theme),
        };

        // Each page carries its own animation identity, so arriving at a page
        // animates rather than only the first page ever doing so.
        let animated = EffectTransition::new(PAGE_TRANSITION)
            .fade(0., 1.)
            .slide_y(px(10.), px(0.))
            .apply(body, page_id(page));

        let mut column = div()
            .flex_1()
            .h_full()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_y_scrollbar()
            .child(
                // The heading sits outside the animated region on purpose: it is
                // the one thing that does not move while the page under it
                // arrives.
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px(px(PAGE_PADDING))
                    .pt(px(PAGE_PADDING))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(page.heading().to_owned()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.colors.muted_foreground)
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .px(px(PAGE_PADDING))
                    .pt_5()
                    .pb(px(PAGE_PADDING))
                    .child(animated),
            );

        // The parts that belong to the session rather than to the page: what a
        // change would do, what a repair left alone, what the update channel
        // said. They appear where the machine produced them rather than behind a
        // menu, because a person who has to ask to see what is about to happen to
        // their machine is not being told.
        let mut extras: Vec<AnyElement> = Vec::new();
        if let Some(plan) = &snapshot.plan {
            extras.push(self.plan(plan, theme).into_any_element());
        }
        extras.extend(drift(&snapshot.repair_drift, theme));
        extras.extend(update(snapshot, theme));
        if !extras.is_empty() {
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .px(px(PAGE_PADDING))
                    .pb(px(PAGE_PADDING))
                    .children(extras),
            );
        }
        column
    }

    /// Scope, components, and location: everything a person chooses.
    fn options(&self, controls: &Controls, theme: &Theme) -> gpui_kit::Div {
        let mut sections = Vec::new();

        if !controls.scopes.is_empty() {
            let session = self.session.clone();
            let mut buttons = Vec::new();
            for choice in &controls.scopes {
                let view = session.clone();
                let action = choice.action.clone();
                let selected = choice.selected;
                let mut button = Button::new(choice.id.clone()).label(choice.label.clone());
                if selected {
                    button = button.primary();
                }
                buttons.push(
                    button
                        .on_click(move |_, _, _| view.send(action.clone()))
                        .into_any_element(),
                );
            }
            sections.push(card(theme, |this| {
                this.flex()
                    .flex_col()
                    .gap_3()
                    .child(section_heading("Who is this for?", theme))
                    // A segmented row rather than a list: a scope is one of a few
                    // choices, and drawing two options as two paragraphs makes
                    // the window look like a document instead of a control.
                    .child(div().flex().flex_col().gap_2().children(buttons))
            }));
        }

        if !controls.components.is_empty() {
            let session = self.session.clone();
            let mut rows = Vec::new();
            for choice in &controls.components {
                let view = session.clone();
                let action = choice.action.clone();
                let mut row = div().flex().flex_col().gap_1().py_1().child(
                    Checkbox::new(choice.id.clone())
                        .label(choice.label.clone())
                        .accessibility_label(if choice.required {
                            format!("{} (required)", choice.label)
                        } else {
                            choice.label.clone()
                        })
                        .checked(choice.selected)
                        .disabled(choice.required)
                        .on_change(move |_, _, _| view.send(action.clone()))
                        .into_any_element(),
                );
                if let Some(description) = &choice.description {
                    row = row.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.colors.muted_foreground)
                            .child(description.clone()),
                    );
                }
                rows.push(row.into_any_element());
            }
            sections.push(card(theme, |this| {
                this.flex()
                    .flex_col()
                    .gap_2()
                    .child(section_heading("What should be installed?", theme))
                    .children(rows)
            }));
        }

        if controls.editable_directory {
            sections.push(card(theme, |this| {
                this.flex()
                    .flex_col()
                    .gap_2()
                    .child(section_heading("Where should it go?", theme))
                    .child(
                        Input::new(&self.location)
                            .aria_label("Install directory")
                            .into_any_element(),
                    )
            }));
        } else if let Some(directory) = &controls.directory {
            sections.push(card(theme, |this| {
                this.flex()
                    .flex_col()
                    .gap_2()
                    .child(section_heading("Where it goes", theme))
                    .child(
                        DescriptionList::vertical()
                            .columns(1)
                            .item("Location", directory.clone(), 1)
                            .into_any_element(),
                    )
            }));
        }

        div().flex().flex_col().gap_4().children(sections)
    }

    /// Work in flight: a position, a bar, and what is happening.
    fn progress(
        &self,
        _snapshot: &UiSnapshot,
        controls: &Controls,
        theme: &Theme,
    ) -> gpui_kit::Div {
        // Borrowed, not cloned: the status is the host's sentence, and a window
        // that keeps a copy of it could disagree with a later snapshot.
        let fallback = Status {
            heading: String::from("Working…"),
            detail: None,
            percent: None,
            cancellable: false,
        };
        let status = controls.status.as_ref().unwrap_or(&fallback);
        card(theme, |this| {
            let mut column = this.flex().flex_col().gap_4();
            if let Some(value) = status.percent {
                column = column.child(
                    div()
                        .text_size(px(40.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{}%", value.round() as i64)),
                );
            }
            column = column.child(
                Progress::new("operation")
                    .loading(status.cancellable && status.percent.is_none_or(|value| value < 100.))
                    .value(status.percent.unwrap_or(0.))
                    .accessibility_label(status.heading.clone())
                    .into_any_element(),
            );
            column = column.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.colors.muted_foreground)
                    .child(status.heading.clone()),
            );
            if let Some(detail) = &status.detail {
                column = column.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.colors.muted_foreground)
                        .child(detail.clone()),
                );
            }
            column
        })
    }

    /// Committed.
    fn done(&self, controls: &Controls, theme: &Theme) -> gpui_kit::Div {
        let heading = controls
            .status
            .as_ref()
            .map_or("Done", |status| status.heading.as_str())
            .to_owned();
        card(theme, |this| {
            this.flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.colors.success)
                        .child(heading.clone()),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.colors.muted_foreground)
                        .child("Close this window, or go back to your application."),
                )
        })
    }

    /// An installation that exists.
    fn maintenance(&self, controls: &Controls, theme: &Theme) -> gpui_kit::Div {
        let mut sections = Vec::new();
        if let Some(directory) = &controls.directory {
            sections.push(card(theme, |this| {
                this.flex()
                    .flex_col()
                    .gap_2()
                    .child(section_heading("Installed at", theme))
                    .child(
                        DescriptionList::vertical()
                            .columns(1)
                            .item("Location", directory.clone(), 1)
                            .into_any_element(),
                    )
            }));
        }
        if !controls.components.is_empty() {
            sections.push(card(theme, |this| {
                let mut list = DescriptionList::vertical().columns(2);
                for choice in &controls.components {
                    list = list.item(
                        choice.label.clone(),
                        if choice.selected {
                            "Installed"
                        } else {
                            "Not installed"
                        }
                        .to_owned(),
                        1,
                    );
                }
                this.flex()
                    .flex_col()
                    .gap_2()
                    .child(section_heading("Installed components", theme))
                    .child(list.into_any_element())
            }));
        }
        div().flex().flex_col().gap_4().children(sections)
    }

    /// Asked to remove it.
    fn confirm(&self, theme: &Theme) -> gpui_kit::Div {
        card(theme, |this| {
            this.flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Remove this application?"),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.colors.muted_foreground)
                        .child(
                            "Its files, its shortcuts and its PATH entry all go. Anything it put \
                             outside its own directory is left alone.",
                        ),
                )
        })
    }

    /// Blocked, failed, or waiting to be reconciled.
    fn problem(&self, snapshot: &UiSnapshot, controls: &Controls, theme: &Theme) -> gpui_kit::Div {
        let heading = controls
            .status
            .as_ref()
            .map_or("The installer cannot continue", |status| {
                status.heading.as_str()
            })
            .to_owned();
        let detail = controls
            .status
            .as_ref()
            .and_then(|status| status.detail.clone());
        let recovery = snapshot
            .diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.recovery.clone());
        card(theme, |this| {
            let mut column = this.flex().flex_col().gap_3().child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.danger)
                    .child(heading.clone()),
            );
            if let Some(detail) = &detail {
                column = column.child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.colors.muted_foreground)
                        .child(detail.clone()),
                );
            }
            if let Some(recovery) = &recovery {
                column = column.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.colors.muted_foreground)
                        .child(recovery.clone()),
                );
            }
            column
        })
    }

    /// What installing would change, on request.
    ///
    /// The plan is asked for rather than shown: on a fresh machine it is empty,
    /// and a window that opens with an empty panel under it is worse than one that
    /// answers a question.
    fn plan(&self, plan: &PlanPreview, theme: &Theme) -> gpui_kit::Div {
        let mut list = DescriptionList::vertical().columns(2);
        list = list.item("Install to", plan.install_directory.clone(), 1);
        list = list.item("Downloads", format_bytes(plan.download_bytes), 1);
        if plan.requires_authorization {
            list = list.item("Needs", "Administrator approval".to_owned(), 1);
        }
        card(theme, |this| {
            this.flex()
                .flex_col()
                .gap_3()
                .child(section_heading("What will change", theme))
                .child(list.into_any_element())
                .children(plan.groups.iter().map(|group| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.colors.foreground)
                                .child(format!(
                                    "{} · {} change{}",
                                    category_label(group.category),
                                    group.changes.len(),
                                    if group.changes.len() == 1 { "" } else { "s" }
                                )),
                        )
                        .children(group.changes.iter().map(|change| {
                            div()
                                .text_size(px(12.))
                                .text_color(theme.colors.muted_foreground)
                                .child(change_label(change))
                                .into_any_element()
                        }))
                        .into_any_element()
                }))
        })
    }

    /// The buttons. Primary last, where a person's eye finishes.
    fn footer(&self, controls: &Controls, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let session = self.session.clone();
        let mut row = Vec::new();

        for (index, (label, action)) in controls.secondary.iter().enumerate() {
            let view = session.clone();
            let action = action.clone();
            row.push(
                Button::new(format!("secondary-{index}"))
                    .label(label.clone())
                    .on_click(move |_, _, _| view.send(action.clone()))
                    .into_any_element(),
            );
        }
        if let Some(action) = &controls.primary {
            let view = session.clone();
            let action = action.clone();
            let directory = self.typed_directory(cx);
            row.push(
                Button::new("primary")
                    .primary()
                    .label(controls.primary_label.clone())
                    .on_click(move |_, _, _| {
                        // A typed location is a separate question from starting the
                        // operation, and the host validates each on its own. Sending
                        // them together would mean a host that refuses the directory
                        // also refuses the install, for two different reasons.
                        if action == UiAction::Install
                            && let Some(directory) = directory.clone()
                        {
                            view.send(UiAction::SetInstallDirectory { directory });
                        }
                        view.send(action.clone());
                    })
                    .into_any_element(),
            );
        }
        let quit = session.clone();
        row.push(
            Button::new("close")
                .label("Close")
                .on_click(move |_, _, _| quit.send(UiAction::Close))
                .into_any_element(),
        );

        div()
            .flex()
            .justify_end()
            .items_center()
            .gap_2()
            .px(px(PAGE_PADDING))
            .py_4()
            .border_t_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.background)
            .children(row)
    }
}

/// The line under a page's name.
fn page_subtitle(page: Page, controls: &Controls) -> String {
    match page {
        Page::Options => "Choose what to install and where to put it.".into(),
        Page::Progress => controls
            .status
            .as_ref()
            .map(|status| status.heading.clone())
            .unwrap_or_else(|| String::from("Working…")),
        Page::Done => "Everything the application asked for is in place.".into(),
        Page::Maintenance => "This application is already installed.".into(),
        Page::Confirm => "This removes the application and everything it registered.".into(),
        Page::Problem => controls
            .status
            .as_ref()
            .and_then(|status| status.detail.clone())
            .unwrap_or_else(|| String::from("The installer cannot continue yet.")),
    }
}

/// The resources a repair declined to overwrite.
fn drift(drift: &[String], theme: &Theme) -> Vec<AnyElement> {
    if drift.is_empty() {
        return Vec::new();
    }
    vec![
        card(theme, |this| {
            this.flex()
                .flex_col()
                .gap_2()
                .child(section_heading(
                    "Left alone because they had drifted",
                    theme,
                ))
                .children(drift.iter().map(|resource| {
                    div()
                        .text_size(px(12.))
                        .text_color(theme.colors.muted_foreground)
                        .child(resource.clone())
                        .into_any_element()
                }))
        })
        .into_any_element(),
    ]
}

/// What the update channel has said.
fn update(snapshot: &UiSnapshot, theme: &Theme) -> Vec<AnyElement> {
    let Some(update) = &snapshot.update else {
        return Vec::new();
    };
    let mut column = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(section_heading("Updates", theme))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.colors.muted_foreground)
                .child(update_label(&update.state)),
        );
    if let Some(channel) = &update.channel {
        column = column.child(
            div()
                .text_size(px(12.))
                .text_color(theme.colors.muted_foreground)
                .child(format!("Channel: {channel}")),
        );
    }
    vec![card(theme, |this| this.child(column)).into_any_element()]
}

fn section_heading(text: &str, theme: &Theme) -> AnyElement {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.colors.foreground)
        .child(text.to_owned())
        .into_any_element()
}

/// A panel with a surface and a border, which is what makes a form read as a
/// document rather than as a list of controls.
fn card(theme: &Theme, build: impl FnOnce(gpui_kit::Div) -> gpui_kit::Div) -> gpui_kit::Div {
    build(div())
        .p_5()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.colors.border)
}

/// The animation identity of a page. Distinct per page, so arriving at one
/// animates rather than only the first page ever doing so.
fn page_id(page: Page) -> &'static str {
    match page {
        Page::Options => "page-options",
        Page::Progress => "page-progress",
        Page::Done => "page-done",
        Page::Maintenance => "page-maintenance",
        Page::Confirm => "page-confirm",
        Page::Problem => "page-problem",
    }
}

/// One resource category, as a person reads it.
///
/// The protocol keeps its own naming out of presentation, so the words a person
/// sees are chosen here rather than baked into the contract every published
/// preset would inherit.
fn category_label(category: ResourceCategory) -> &'static str {
    match category {
        ResourceCategory::Files => "Files",
        ResourceCategory::Launchers => "Start menu and desktop shortcuts",
        ResourceCategory::Path => "The PATH environment variable",
        ResourceCategory::Services => "Windows services",
        ResourceCategory::Protocols => "File type handlers",
        ResourceCategory::FileAssociations => "File associations",
        ResourceCategory::AppsFeatures => "Apps & Features",
        ResourceCategory::Maintenance => "Uninstall information",
        ResourceCategory::Prerequisites => "Required components",
        ResourceCategory::Other => "Other",
    }
}

/// One planned change, as a person reads it.
fn change_label(change: &PlannedChange) -> String {
    let location = change.location.clone().unwrap_or_default();
    match (location.is_empty(), change.component.as_ref()) {
        (false, Some(component)) => format!("{} — {component}", change.label),
        (false, None) => format!("{} — {location}", change.label),
        (true, _) => change.label.clone(),
    }
}

/// What the update channel has got to.
fn update_label(state: &UpdateState) -> String {
    match state {
        UpdateState::Idle => "No check has run yet.".into(),
        UpdateState::Checking { detail } => format!("Checking… {detail}"),
        UpdateState::Installing { detail } => format!("Installing… {detail}"),
        UpdateState::UpToDate { current } => format!("Up to date ({current})."),
        UpdateState::Available { current, available } => {
            format!("{available} is available. You have {current}.")
        }
        UpdateState::Failed { message } => format!("The update check failed: {message}"),
    }
}

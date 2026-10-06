//! The window, and the screens it shows.
//!
//! One screen per lifecycle state, chosen by [`Screen::of`]. Nothing here
//! decides what the installation is doing; it decides how that looks, and keeps
//! the overlays - the plan sheet and the uninstall confirmation - in step with
//! the state that owns them.

use std::collections::BTreeSet;

use zup_preset_sdk::gpui_kit::assets::IconName;
use zup_preset_sdk::gpui_kit::component::animation::EffectTransition;
use zup_preset_sdk::gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use zup_preset_sdk::gpui_kit::component::scroll::ScrollableElement;
use zup_preset_sdk::gpui_kit::component::spinner::Spinner;
use zup_preset_sdk::gpui_kit::component::{
    ActiveTheme, Disableable, Icon, Sizable, TitleBar, WindowExt, h_flex, v_flex,
};
use zup_preset_sdk::gpui_kit::prelude::FluentBuilder as _;
use zup_preset_sdk::gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Render, SharedString, Styled, Subscription,
    Window, WindowBounds, WindowOptions, div, px, relative, size,
};
use zup_preset_sdk::prelude::*;
use zup_preset_sdk::presentation::ResourceCategory;

use crate::Settings;
use crate::model::{self, Screen};
use crate::present;
use crate::theme::{self, Layout, motion, space, text};
use crate::ui::plan::{PlanDetails, has_details};
use crate::ui::{
    ActionBar, ActionRow, AppIdentity, AppMark, BlockedView, Callout, ComponentChoice,
    DestructiveSection, DiagnosticView, Disclosure, Emphasis, GroupSummary, Handler, HealthNotice,
    InstallSummary, OperationProgress, OutcomeView, PathChooser, ScopeChoice, Section, Tone,
    UpdateRow, caption, handler, muted,
};

/// The gallery renders many states in one process, so closing a shot must not
/// end it. The real installer still quits when its window closes.
static KEEP_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Keep the process alive when the window closes.
pub fn keep_process_on_close() {
    KEEP_OPEN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Open the installer window for a session the SDK has already started.
pub fn open(context: PresetContext<Settings>, cx: &mut App) {
    let session = context.session().clone();
    let settings: Entity<Settings> = (**context.settings()).clone();
    let capabilities = context.capabilities().clone();
    let options = window_options(cx);
    zup_preset_sdk::gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| Installer::new(session, settings, capabilities, window, cx))
    })
    .expect("open the installer window");
}

/// The window's size, place and chrome.
pub fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(theme::window::WIDTH, theme::window::HEIGHT),
            cx,
        ))),
        window_min_size: Some(size(theme::window::MIN_WIDTH, theme::window::MIN_HEIGHT)),
        ..TitleBar::window_options()
    }
}

/// Something the window can show beyond its resting layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// The install screen's customization.
    Customize,
    /// The maintenance screen's component choices.
    Modify,
    /// A problem's technical details.
    Technical,
    /// The sheet of what will change, with every group open.
    Plan,
}

/// Presentation state: what is open, and nothing about the installation.
#[derive(Default)]
struct Local {
    customize_open: bool,
    /// The maintenance screen is showing its component choices.
    modifying: bool,
    technical_open: bool,
    plan_groups: BTreeSet<ResourceCategory>,
    /// The screen the window last showed, so leaving one resets what belonged
    /// to it.
    screen: Option<Screen>,
    /// The confirmation was answered and the host has not moved on yet.
    confirm_answered: bool,
}

/// The installer window.
pub struct Installer {
    session: Session,
    settings: Entity<Settings>,
    capabilities: Capabilities,
    local: Local,
    _subscriptions: Vec<Subscription>,
}

impl Installer {
    pub fn new(
        session: Session,
        settings: Entity<Settings>,
        capabilities: Capabilities,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = session.state();
        let subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| this.sync(window, cx)),
            cx.observe_in(&settings, window, |this, _, window, cx| {
                let settings = this.settings.read(cx).clone();
                theme::apply(&settings, window, cx);
                cx.notify();
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                let settings = this.settings.read(cx).clone();
                theme::apply(&settings, window, cx);
            }),
        ];
        {
            let session = session.clone();
            window.on_window_should_close(cx, move |_, cx| {
                let active = session
                    .state()
                    .read(cx)
                    .snapshot()
                    .is_some_and(|snapshot| snapshot.state.is_active());
                if active {
                    // Closing mid-operation is a request to stop, not a way to
                    // walk away from a half-changed machine.
                    session.send(Action::Cancel);
                    return false;
                }
                session.send(Action::Close);
                if !KEEP_OPEN.load(std::sync::atomic::Ordering::Relaxed) {
                    cx.defer(|cx| cx.quit());
                }
                true
            });
        }
        let current = settings.read(cx).clone();
        theme::apply(&current, window, cx);
        let title = session
            .state()
            .read(cx)
            .snapshot()
            .map(|snapshot| format!("{} Setup", snapshot.product.name));
        if let Some(title) = title {
            window.set_window_title(&title);
        }
        cx.defer_in(window, |this, window, cx| this.sync(window, cx));
        Self {
            session,
            settings,
            capabilities,
            local: Local::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Open one part of the window, as a person pressing its control would.
    pub fn reveal(&mut self, reveal: Reveal, window: &mut Window, cx: &mut Context<Self>) {
        match reveal {
            Reveal::Customize => self.local.customize_open = true,
            Reveal::Modify => self.local.modifying = true,
            Reveal::Technical => self.local.technical_open = true,
            Reveal::Plan => {
                if let Some(plan) = self
                    .snapshot(cx)
                    .and_then(|snapshot| snapshot.plan.latest().cloned())
                {
                    self.local.plan_groups =
                        plan.groups.iter().map(|group| group.category).collect();
                }
                self.open_plan(window, cx);
            }
        }
        cx.notify();
    }

    fn snapshot(&self, cx: &App) -> Option<Snapshot> {
        self.session.state().read(cx).snapshot().cloned()
    }

    fn logo(&self, cx: &App) -> Option<SharedString> {
        self.settings
            .read(cx)
            .logo
            .as_ref()
            .map(|logo| SharedString::from(logo.as_str().to_owned()))
    }

    /// Keep the overlays and the presentation state in step with the host.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot(cx) else {
            cx.notify();
            return;
        };
        let screen = Screen::of(&snapshot);
        if self.local.screen != Some(screen) {
            self.local.screen = Some(screen);
            self.local.technical_open = false;
            if screen != Screen::Maintenance {
                self.local.modifying = false;
            }
        }
        if window.has_active_sheet(cx) && !matches!(screen, Screen::Install | Screen::Maintenance) {
            window.close_sheet(cx);
        }
        let confirming = snapshot.state == InstallerState::ConfirmUninstall;
        if !confirming {
            self.local.confirm_answered = false;
            if window.has_active_dialog(cx) {
                window.close_all_dialogs(cx);
            }
        } else if !self.local.confirm_answered && !window.has_active_dialog(cx) {
            self.confirm_uninstall(&snapshot, window, cx);
        }
        cx.notify();
    }

    /// A control that sends one action.
    fn send(&self, action: Action) -> Handler {
        let session = self.session.clone();
        handler(move |_, _| session.send(action.clone()))
    }

    /// A control that changes this window's own state.
    fn local(
        &self,
        cx: &Context<Self>,
        change: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Handler {
        let view = cx.entity();
        handler(move |window, cx| {
            view.update(cx, |this, cx| {
                change(this, window, cx);
                cx.notify();
            })
        })
    }

    fn close(&self) -> Handler {
        let session = self.session.clone();
        handler(move |_, cx| {
            session.send(Action::Close);
            cx.quit();
        })
    }

    /// Ask the system for a folder, and ask the host to install there.
    fn choose_location(&mut self, product: String, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Install here".into()),
        });
        let session = self.session.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(chosen) = paths.into_iter().next()
            {
                session.send(Action::SetInstallDirectory {
                    directory: model::folder_for(&chosen, &product),
                });
            }
        })
        .detach();
    }

    fn confirm_uninstall(
        &mut self,
        snapshot: &Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = snapshot.product.name.clone();
        let location = snapshot.surface.install_directory().map(str::to_owned);
        let session = self.session.clone();
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let (confirm, dismiss) = (session.clone(), session.clone());
            let (confirmed, dismissed) = (view.clone(), view.clone());
            alert
                .width(px(440.))
                .icon(
                    div()
                        .flex_shrink_0()
                        .size(space::XXL)
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(theme
                            .danger
                            .opacity(if theme.is_dark() { 0.18 } else { 0.1 }))
                        .child(
                            Icon::new(IconName::Trash)
                                .size(space::LG)
                                .text_color(theme.danger),
                        ),
                )
                .title(format!("Uninstall {name}?"))
                .description(format!(
                    "{name} and everything setup added to this computer will be removed, \
                     including its shortcuts and its entry in Settings › Apps."
                ))
                .child(
                    v_flex()
                        .gap(space::SM)
                        .mt(space::SM)
                        .px(space::MD)
                        .py(space::MD)
                        .rounded(theme.radius)
                        .bg(theme.muted.opacity(0.55))
                        .child(
                            div()
                                .text_size(text::SMALL)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground)
                                .child("Your documents and settings stay"),
                        )
                        .child(caption(
                            format!(
                                "Files {name} created outside its install folder aren't removed."
                            ),
                            cx,
                        ))
                        .children(location.clone().map(|location| {
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis_middle()
                                .text_size(text::CAPTION)
                                .text_color(theme.muted_foreground)
                                .child(format!("Removed: {location}"))
                        })),
                )
                .confirm()
                .ok_text("Uninstall")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Cancel")
                .on_ok(move |_, _, cx| {
                    confirmed.update(cx, |this, _| this.local.confirm_answered = true);
                    confirm.send(Action::ConfirmUninstall);
                    true
                })
                .on_cancel(move |_, _, cx| {
                    dismissed.update(cx, |this, _| this.local.confirm_answered = true);
                    dismiss.send(Action::DismissUninstall);
                    true
                })
        });
    }

    /// Show what the current choices would change.
    fn open_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        let width = theme::window::SHEET.min(window.viewport_size().width - px(24.));
        window.open_sheet(cx, move |sheet, _, cx| {
            let this = view.read(cx);
            let Some(snapshot) = this.snapshot(cx) else {
                return sheet;
            };
            let expanded = this.local.plan_groups.clone();
            let modifying = this.local.modifying;
            let toggle = view.clone();
            let primary = match (&snapshot.state, modifying) {
                (InstallerState::Options, _) => {
                    Some((model::install_label(&snapshot), Action::Install))
                }
                (InstallerState::Maintenance, true)
                    if model::pending_sentence(&snapshot.surface).is_some() =>
                {
                    Some(("Apply changes", Action::Modify))
                }
                _ => None,
            };
            let session = this.session.clone();
            let elevated = model::needs_approval(&snapshot);
            sheet
                .title("What will change")
                .size(width)
                .resizable(false)
                .child(PlanDetails::new(
                    snapshot.plan.clone(),
                    model::review_context(&snapshot),
                    expanded,
                    move |category, _, cx| {
                        toggle.update(cx, |this, cx| {
                            if !this.local.plan_groups.remove(&category) {
                                this.local.plan_groups.insert(category);
                            }
                            cx.notify();
                        });
                    },
                ))
                .when_some(primary, |sheet, (label, action)| {
                    sheet.footer(
                        h_flex().w_full().justify_end().child(
                            Button::new("plan-primary")
                                .primary()
                                .label(label)
                                .when(elevated, |button| button.icon(IconName::ShieldAlert))
                                .on_click(move |_, window, cx| {
                                    window.close_sheet(cx);
                                    session.send(action.clone());
                                }),
                        ),
                    )
                })
        });
    }

    fn plan_handler(&self, cx: &Context<Self>) -> Handler {
        self.local(cx, |this, window, cx| this.open_plan(window, cx))
    }

    // -- Screens ------------------------------------------------------------

    fn install(
        &self,
        snapshot: &Snapshot,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ActionBar) {
        let logo = self.logo(cx);
        let surface = present::InstallSurface::of(snapshot);
        let blocked = surface.blocked();
        let mut body = v_flex()
            .gap(space::XL)
            .child(
                AppIdentity::new(&snapshot.product, logo)
                    .note(model::install_subtitle(snapshot))
                    .compact(layout.is_compact()),
            )
            .children(self.resting_notice(snapshot, cx));
        for group in &surface.primary {
            body = body.child(self.group_block(group, cx));
        }
        if surface.show_location {
            let product = snapshot.product.name.clone();
            let choose = self.local(cx, move |this, _, cx| {
                this.choose_location(product.clone(), cx)
            });
            body = body.child(
                PathChooser::new("Install location", model::location(snapshot))
                    .on_change(choose)
                    .on_reset(self.send(Action::ResetInstallDirectory)),
            );
        }
        if surface.show_scope || !surface.secondary.is_empty() {
            let mut disclosure = Disclosure::new(
                "customize",
                "Customize installation",
                self.local.customize_open,
                self.local(cx, |this, _, _| this.local.customize_open ^= true),
            )
            .summary(surface.customize_summary(snapshot.surface.scope()));
            if surface.show_scope {
                disclosure = disclosure.children(self.scope_section(snapshot));
            }
            for group in &surface.secondary {
                disclosure = disclosure.child(self.group_block(group, cx));
            }
            body = body.child(disclosure);
        }

        let elevated = model::needs_approval(snapshot);
        let mut bar =
            ActionBar::new(layout).leading(InstallSummary::new(model::commit_facts(snapshot)));
        if has_details(&snapshot.plan) {
            let open_plan = self.plan_handler(cx);
            bar = bar.trailing(
                Button::new("what-changes")
                    .ghost()
                    .label("Review changes")
                    .on_click(move |_, window, cx| open_plan(window, cx)),
            );
        }
        let install = self.send(Action::Install);
        bar = bar.trailing(
            Button::new("install")
                .primary()
                .label(model::install_label(snapshot))
                .min_w(theme::size::ACTION_MIN)
                .disabled(blocked.is_some())
                .when(elevated && blocked.is_none(), |button| {
                    button
                        .icon(IconName::ShieldAlert)
                        .tooltip("The system will ask for administrator approval")
                })
                .when_some(blocked, |button, reason| button.tooltip(reason))
                .on_click(move |_, window, cx| install(window, cx)),
        );
        (body.into_any_element(), bar)
    }

    fn group_block(&self, group: &present::ResolvedGroup, cx: &Context<Self>) -> AnyElement {
        if group.inline() {
            return Self::component_list(&self.session, &group.title, &group.rows, cx);
        }
        let rows = group.rows.clone();
        let title = group.title.clone();
        GroupSummary::new(
            group.title.clone(),
            group.count(),
            group.selected_names(),
            self.local(cx, move |this, window, cx| {
                this.open_group(title.clone(), rows.clone(), window, cx);
            }),
        )
        .into_any_element()
    }

    fn open_group(
        &mut self,
        title: String,
        rows: Vec<model::ComponentRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.clone();
        window.open_sheet(cx, move |sheet, _, cx| {
            let list = Self::component_list(&session, &title, &rows, cx);
            sheet.title(title.clone()).child(list).resizable(false)
        });
    }

    fn component_list(
        session: &Session,
        label: &str,
        rows: &[model::ComponentRow],
        cx: &App,
    ) -> AnyElement {
        let chosen = rows
            .iter()
            .filter(|row| match row.kind {
                model::ComponentKind::Required => true,
                model::ComponentKind::Optional { selected } => selected,
            })
            .count();
        let total = rows.len();
        Section::new(label.to_owned())
            .trailing(caption(format!("{chosen} of {total} selected"), cx))
            .child(
                v_flex()
                    .gap(space::HAIR)
                    .children(rows.iter().cloned().map(|row| {
                        let toggle = match row.toggle() {
                            Some(action) => {
                                let session = session.clone();
                                handler(move |_, _| session.send(action.clone()))
                            }
                            None => handler(|_, _| {}),
                        };
                        ComponentChoice::new(row, toggle)
                    })),
            )
            .into_any_element()
    }

    fn scope_section(&self, snapshot: &Snapshot) -> Option<AnyElement> {
        let choices = model::scope_choices(snapshot);
        if choices.is_empty() {
            return None;
        }
        let session = self.session.clone();
        Some(
            Section::new("Install for")
                .child(ScopeChoice::new(
                    choices,
                    snapshot.surface.scope(),
                    move |scope, _, _| session.send(Action::SetScope { scope }),
                ))
                .into_any_element(),
        )
    }

    /// A message the host left on a screen that is waiting for a person.
    fn resting_notice(&self, snapshot: &Snapshot, _: &App) -> Option<AnyElement> {
        let diagnostic = snapshot.diagnostic.as_ref()?;
        Some(
            Callout::new(Tone::Attention, IconName::Info)
                .title(diagnostic.title.clone())
                .body(format!("{} {}", diagnostic.meaning, diagnostic.recovery))
                .into_any_element(),
        )
    }

    fn maintenance(
        &self,
        snapshot: &Snapshot,
        state: &MaintenanceState,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ActionBar) {
        let name = snapshot.product.name.clone();
        let byline = match &snapshot.product.publisher {
            Some(publisher) => format!("{publisher} · Version {}", state.installed_version),
            None => format!("Version {}", state.installed_version),
        };
        let mut body = v_flex()
            .gap(space::XL)
            .child(
                AppIdentity::new(&snapshot.product, self.logo(cx))
                    .byline(byline)
                    .note(Some(model::audience(state.scope).to_owned()))
                    .compact(layout.is_compact()),
            )
            .children(self.resting_notice(snapshot, cx))
            .child(PathChooser::new(
                "Location",
                model::Location {
                    path: state.install_directory.clone(),
                    custom: false,
                    changeable: false,
                },
            ));

        let health = model::health(state);
        if let model::Health::Drifted(resources) = &health
            && !self.local.modifying
        {
            body = body.child(HealthNotice::new(
                resources.clone(),
                self.send(Action::Repair),
            ));
        }

        if self.local.modifying {
            let mut bar = ActionBar::new(layout);
            let pending = model::pending_sentence(&snapshot.surface);
            body = body
                .children(
                    present::resolve(snapshot)
                        .into_iter()
                        .map(|group| self.group_block(&group, cx))
                        .collect::<Vec<_>>(),
                )
                .child(match &pending {
                    Some(sentence) => Callout::new(Tone::Neutral, IconName::Info)
                        .body(sentence.clone())
                        .into_any_element(),
                    None => muted("Choose what to add or remove.", cx).into_any_element(),
                });
            if has_details(&snapshot.plan) {
                let open_plan = self.plan_handler(cx);
                bar = bar.leading(
                    Button::new("what-changes")
                        .ghost()
                        .label("Review changes")
                        .on_click(move |_, window, cx| open_plan(window, cx)),
                );
            }
            let undo: Vec<Action> = state
                .components
                .iter()
                .filter(|component| component.selected != component.installed)
                .map(|component| Action::SetComponent {
                    component: component.id.clone(),
                    selected: component.installed,
                })
                .collect();
            let session = self.session.clone();
            let view = cx.entity();
            let apply = self.send(Action::Modify);
            bar =
                bar.trailing(Button::new("modify-cancel").label("Cancel").on_click(
                    move |_, _, cx| {
                        for action in &undo {
                            session.send(action.clone());
                        }
                        view.update(cx, |this, cx| {
                            this.local.modifying = false;
                            cx.notify();
                        });
                    },
                ))
                .trailing(
                    Button::new("modify-apply")
                        .primary()
                        .label("Apply changes")
                        .min_w(theme::size::ACTION_MIN)
                        .disabled(pending.is_none())
                        .when(model::needs_approval(snapshot), |button| {
                            button.icon(IconName::ShieldAlert)
                        })
                        .on_click(move |_, window, cx| apply(window, cx)),
                );
            return (body.into_any_element(), bar);
        }

        let mut actions = v_flex();
        if let Some(row) = model::update_row(snapshot) {
            actions = actions.child(UpdateRow::new(row, layout, self.send(Action::Update)));
        }
        if model::has_optional_components(&snapshot.surface) {
            actions = actions.child(
                ActionRow::new(
                    "modify",
                    IconName::Package,
                    "Change components",
                    format!("Add or remove optional parts of {name}."),
                )
                .layout(layout)
                .button(
                    "Change",
                    Emphasis::Normal,
                    self.local(cx, |this, _, _| this.local.modifying = true),
                ),
            );
        }
        actions = actions.child(
            ActionRow::new(
                "repair",
                IconName::Wrench,
                "Repair",
                "Restore the files and settings setup manages, if something went missing or \
                 stopped working.",
            )
            .layout(layout)
            .button("Repair", Emphasis::Normal, self.send(Action::Repair)),
        );
        body = body
            .child(Section::new("Manage").child(actions))
            .child(DestructiveSection::new(
                format!("Uninstall {name}"),
                "Remove the app and everything setup added to this computer.",
                "Uninstall…",
                layout,
                self.send(Action::RequestUninstall),
            ));
        (body.into_any_element(), ActionBar::new(layout))
    }

    fn operation(
        &self,
        snapshot: &Snapshot,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ActionBar) {
        let progress = model::progress(snapshot);
        let stopping = progress.stopping;
        let body = OperationProgress::new(snapshot.product.name.clone(), self.logo(cx), progress)
            .into_any_element();
        let stop = self.send(Action::Cancel);
        let bar = ActionBar::new(layout).trailing(
            Button::new("stop")
                .label(if stopping { "Stopping…" } else { "Stop" })
                .disabled(stopping)
                .on_click(move |_, window, cx| stop(window, cx)),
        );
        (body, bar)
    }

    fn outcome(
        &self,
        snapshot: &Snapshot,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ActionBar) {
        let outcome = model::outcome(snapshot);
        let launch = outcome.launch.clone();
        let body = OutcomeView::new(snapshot.product.name.clone(), self.logo(cx), outcome)
            .into_any_element();
        let close = self.close();
        let mut bar = ActionBar::new(layout).trailing(
            Button::new("done")
                .label("Close")
                .when(launch.is_none(), |button| {
                    button.primary().min_w(theme::size::ACTION_MIN)
                })
                .on_click(move |_, window, cx| close(window, cx)),
        );
        if let Some(name) = launch {
            let launch = self.send(Action::Launch);
            let close = self.close();
            bar = bar.trailing(
                Button::new("launch")
                    .primary()
                    .label(format!("Open {name}"))
                    .icon(IconName::Play)
                    .on_click(move |_, window, cx| {
                        launch(window, cx);
                        close(window, cx);
                    }),
            );
        }
        (body, bar)
    }

    fn blocked(&self, snapshot: &Snapshot, layout: Layout) -> (AnyElement, ActionBar) {
        let body = BlockedView::new(model::blocked(snapshot)).into_any_element();
        let close = self.close();
        let retry = self.send(Action::Retry);
        let bar = ActionBar::new(layout)
            .trailing(
                Button::new("close")
                    .label("Close setup")
                    .on_click(move |_, window, cx| close(window, cx)),
            )
            .trailing(
                Button::new("retry")
                    .primary()
                    .label("Try again")
                    .icon(IconName::RotateCcw)
                    .min_w(theme::size::ACTION_MIN)
                    .on_click(move |_, window, cx| retry(window, cx)),
            );
        (body, bar)
    }

    fn problem(
        &self,
        snapshot: &Snapshot,
        layout: Layout,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ActionBar) {
        let problem = model::problem(snapshot);
        let copy = self.send(Action::CopyDiagnostics);
        let mut view = DiagnosticView::new(
            problem,
            self.local.technical_open,
            self.local(cx, |this, _, _| this.local.technical_open ^= true),
        );
        if self.capabilities.contains(Capability::Diagnostics) {
            let open_log = self.send(Action::OpenLog);
            view = view
                .support(
                    Button::new("copy-details")
                        .ghost()
                        .small()
                        .icon(IconName::ClipboardCopy)
                        .label("Copy details")
                        .on_click(move |_, window, cx| copy(window, cx)),
                )
                .support(
                    Button::new("open-log")
                        .ghost()
                        .small()
                        .icon(IconName::ScrollText)
                        .label("Open log")
                        .on_click(move |_, window, cx| open_log(window, cx)),
                );
        }
        let close = self.close();
        let retry = self.send(Action::Retry);
        let bar = ActionBar::new(layout)
            .trailing(
                Button::new("close")
                    .label("Close setup")
                    .on_click(move |_, window, cx| close(window, cx)),
            )
            .trailing(
                Button::new("retry")
                    .primary()
                    .label("Try again")
                    .icon(IconName::RotateCcw)
                    .min_w(theme::size::ACTION_MIN)
                    .on_click(move |_, window, cx| retry(window, cx)),
            );
        (view.into_any_element(), bar)
    }

    fn title_bar(&self, snapshot: Option<&Snapshot>, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let name = snapshot.map_or_else(String::new, |snapshot| snapshot.product.name.clone());
        TitleBar::new()
            .bg(theme.background)
            .border_color(theme.background)
            .child(
                h_flex()
                    .gap(space::SM)
                    .when(!name.is_empty(), |this| {
                        this.child(
                            AppMark::new(name.clone(), self.logo(cx)).size(theme::size::MARK_TITLE),
                        )
                    })
                    .child(
                        div()
                            .text_size(text::SMALL)
                            .text_color(theme.muted_foreground)
                            .child(if name.is_empty() {
                                "Setup".to_owned()
                            } else {
                                format!("{name} Setup")
                            }),
                    ),
            )
    }

    fn waiting(&self, cx: &App) -> AnyElement {
        let closed = self.session.state().read(cx).closed().map(str::to_owned);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(space::MD)
            .child(match &closed {
                Some(_) => Icon::new(IconName::CircleAlert)
                    .size(theme::size::ICON_LG)
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
                None => Spinner::new().into_any_element(),
            })
            .child(muted(
                match closed {
                    Some(reason) => format!("Setup has closed: {reason}"),
                    None => "Waiting for setup…".to_owned(),
                },
                cx,
            ))
            .into_any_element()
    }
}

impl Render for Installer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = Layout::of(window);
        let snapshot = self.snapshot(cx);
        let title_bar = self.title_bar(snapshot.as_ref(), cx).into_any_element();
        let theme = cx.theme().clone();
        let root = v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar);
        let Some(snapshot) = snapshot else {
            return root.child(self.waiting(cx));
        };

        let screen = Screen::of(&snapshot);
        let (body, bar) = match screen {
            Screen::Install => self.install(&snapshot, layout, cx),
            Screen::Maintenance => match &snapshot.surface {
                Surface::Maintenance(state) => self.maintenance(&snapshot, state, layout, cx),
                Surface::Install(_) => self.install(&snapshot, layout, cx),
            },
            Screen::Operation => self.operation(&snapshot, layout, cx),
            Screen::Outcome => self.outcome(&snapshot, layout, cx),
            Screen::Blocked => self.blocked(&snapshot, layout),
            Screen::Problem => self.problem(&snapshot, layout, cx),
        };
        let focused = matches!(screen, Screen::Operation | Screen::Outcome);
        let column = div()
            .w_full()
            .max_w(if focused {
                theme::size::FOCUS_COLUMN
            } else {
                theme::size::COLUMN
            })
            .mx_auto()
            .child(body);
        let entering = EffectTransition::new(motion::ENTER)
            .fade(0., 1.)
            .slide_y(motion::ENTER_DISTANCE, px(0.))
            .apply(column, SharedString::from(format!("screen-{screen:?}")));
        let content = v_flex()
            .id("content")
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .min_h_full()
                    .px(layout.gutter())
                    .py(if layout.is_compact() {
                        space::LG
                    } else {
                        space::XL
                    })
                    .when(focused, |this| this.justify_center())
                    .child(entering),
            )
            // A new screen starts at the top. The scroll id is the screen, so a
            // long page cannot leave the next, shorter one scrolled past its content.
            .overflow_y_scrollbar()
            .id(SharedString::from(format!("screen-scroll-{screen:?}")));

        root.child(content)
            .when(!bar.is_empty(), |this| this.child(bar))
            .line_height(relative(1.4))
    }
}

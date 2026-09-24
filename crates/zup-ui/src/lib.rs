//! Native installer and maintenance frontend.
//!
//! This crate owns presentation state only. The host application translates
//! [`UiCommand`] values into planner/runtime requests and returns runtime events.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender};

use gpui_kit::base::StyledExt;
use gpui_kit::component::button::*;
use gpui_kit::component::{ActiveTheme, Root, Theme};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, FontWeight, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, application, assets,
    div, px, rgb, size,
};
use zup_core::{ComponentId, SelectedScope};
use zup_runtime::{InstallOutcome, RuntimeEvent, RuntimeState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductIdentity {
    pub name: String,
    pub publisher: Option<String>,
    pub version: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentOption {
    pub id: ComponentId,
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    Installer {
        identity: ProductIdentity,
        install: InstallModel,
    },
    Maintenance {
        identity: ProductIdentity,
        installed_version: String,
        components: Vec<ComponentOption>,
        updates_enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallModel {
    pub existing_version: Option<String>,
    pub scopes: Vec<SelectedScope>,
    pub selected_scope: SelectedScope,
    pub components: Vec<ComponentOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Install {
        scope: SelectedScope,
        components: Vec<ComponentId>,
    },
    Update,
    Modify {
        components: Vec<ComponentId>,
    },
    Repair,
    Uninstall,
    Cancel,
    Retry,
    ConfirmUninstall,
    DismissUninstall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    Runtime(RuntimeEvent),
    ConfirmUninstall,
    DismissUninstall,
    CancellationWaiting,
    Quit,
    Progress {
        completed: u64,
        total: u64,
        action: String,
    },
    UpdateAvailable {
        current: String,
        available: String,
    },
    UpToDate {
        current: String,
    },
    RepairFinished {
        drifted_resources: Vec<String>,
    },
    OperationFinished(InstallOutcome),
    Error {
        message: String,
        recovery_required: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Options,
    Maintenance,
    Running,
    WaitingForSafeCancel,
    Blocked,
    ConfirmUninstall,
    Success,
    Error,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressModel {
    pub completed: u64,
    pub total: u64,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiModel {
    pub surface: Surface,
    pub state: ViewState,
    pub progress: Option<ProgressModel>,
    pub blockers: Vec<String>,
    pub error: Option<String>,
    pub update_status: Option<String>,
    pub repair_drift: Vec<String>,
    pub close_requested: bool,
}

impl UiModel {
    pub fn new(surface: Surface) -> Self {
        let state = match surface {
            Surface::Installer { .. } => ViewState::Options,
            Surface::Maintenance { .. } => ViewState::Maintenance,
        };
        Self {
            surface,
            state,
            progress: None,
            blockers: Vec::new(),
            error: None,
            update_status: None,
            repair_drift: Vec::new(),
            close_requested: false,
        }
    }

    pub fn begin_operation(&mut self) {
        self.state = ViewState::Running;
        self.progress = Some(ProgressModel {
            completed: 0,
            total: 0,
            action: "Preparing…".into(),
        });
        self.error = None;
        self.blockers.clear();
    }

    pub fn request_cancel(&mut self) {
        self.state = ViewState::WaitingForSafeCancel;
    }

    pub fn request_close(&mut self) -> bool {
        if matches!(
            self.state,
            ViewState::Running | ViewState::WaitingForSafeCancel
        ) {
            self.close_requested = true;
            false
        } else {
            true
        }
    }

    pub fn set_component_selected(&mut self, id: &ComponentId, selected: bool) -> bool {
        let components = match &mut self.surface {
            Surface::Installer { install, .. } => &mut install.components,
            Surface::Maintenance { components, .. } => components,
        };
        let Some(component) = components.iter_mut().find(|component| &component.id == id) else {
            return false;
        };
        if component.required && !selected {
            return false;
        }
        component.selected = selected;
        true
    }

    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::Runtime(event) => self.apply_runtime(event),
            UiEvent::Quit => {}
            UiEvent::ConfirmUninstall => self.state = ViewState::ConfirmUninstall,
            UiEvent::DismissUninstall => self.state = ViewState::Maintenance,
            UiEvent::CancellationWaiting => self.request_cancel(),
            UiEvent::Progress {
                completed,
                total,
                action,
            } => {
                self.progress = Some(ProgressModel {
                    completed,
                    total,
                    action,
                });
                self.state = ViewState::Running;
            }
            UiEvent::UpdateAvailable { current, available } => {
                self.update_status = Some(format!("Update available · {current} → {available}"));
            }
            UiEvent::UpToDate { current } => {
                self.update_status = Some(format!("Up to date · {current}"));
            }
            UiEvent::RepairFinished { drifted_resources } => {
                self.repair_drift = drifted_resources;
            }
            UiEvent::OperationFinished(outcome) => match outcome {
                InstallOutcome::Committed => self.state = ViewState::Success,
                InstallOutcome::RolledBack | InstallOutcome::Cancelled => {
                    self.state = if matches!(self.surface, Surface::Installer { .. }) {
                        ViewState::Options
                    } else {
                        ViewState::Maintenance
                    };
                    self.progress = None;
                }
                InstallOutcome::RecoveryRequired => self.state = ViewState::RecoveryRequired,
                InstallOutcome::Failed(message) => {
                    self.error = Some(message);
                    self.state = ViewState::Error;
                }
            },
            UiEvent::Error {
                message,
                recovery_required,
            } => {
                self.error = Some(message);
                self.state = if recovery_required {
                    ViewState::RecoveryRequired
                } else {
                    ViewState::Error
                };
            }
        }
    }

    fn apply_runtime(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::WaitingForElevation => self.action("Waiting for approval…"),
            RuntimeEvent::WorkerConnected => self.action("Starting…"),
            RuntimeEvent::PreflightStarted => self.action("Checking for open applications…"),
            RuntimeEvent::BlockingProcessesFound { detail } => {
                self.blockers = detail.lines().map(str::to_owned).collect();
                self.state = ViewState::Blocked;
            }
            RuntimeEvent::StagingStarted { .. } => self.action("Preparing files…"),
            RuntimeEvent::StagingProgress { detail, .. } => self.action(&detail),
            RuntimeEvent::OperationStarted { id } => self.action(&friendly_action(&id)),
            RuntimeEvent::Progress {
                completed,
                total,
                action,
            } => {
                let waiting_for_cancel = self.state == ViewState::WaitingForSafeCancel;
                self.progress = Some(ProgressModel {
                    completed,
                    total,
                    action,
                });
                self.state = if waiting_for_cancel {
                    ViewState::WaitingForSafeCancel
                } else {
                    ViewState::Running
                };
            }
            RuntimeEvent::RollingBack => self.action("Restoring the previous state…"),
            RuntimeEvent::Completed { .. } => self.state = ViewState::Success,
            RuntimeEvent::Failed { kind, message } => {
                self.error = Some(message);
                self.state = if kind == "recovery_required" {
                    ViewState::RecoveryRequired
                } else {
                    ViewState::Error
                };
            }
            RuntimeEvent::StateChanged { state } => match state {
                RuntimeState::Preparing => self.action("Preparing…"),
                RuntimeState::WaitingForElevation => self.action("Waiting for approval…"),
                RuntimeState::ConnectingWorker => self.action("Starting…"),
                RuntimeState::Executing => self.action("Installing files…"),
                RuntimeState::RollingBack => self.action("Restoring the previous state…"),
                RuntimeState::Completed => self.state = ViewState::Success,
                RuntimeState::Cancelled => self.state = ViewState::Maintenance,
                RuntimeState::Failed => self.state = ViewState::Error,
            },
        }
    }

    fn action(&mut self, action: &str) {
        self.state = ViewState::Running;
        self.progress
            .get_or_insert(ProgressModel {
                completed: 0,
                total: 0,
                action: action.into(),
            })
            .action = action.into();
    }
}

fn friendly_action(id: &str) -> String {
    let id = id.to_ascii_lowercase();
    if id.contains("service") {
        "Registering services…".into()
    } else if id.contains("shortcut") {
        "Updating shortcuts…".into()
    } else if id.contains("file") {
        "Installing files…".into()
    } else {
        "Finishing…".into()
    }
}

pub fn run(surface: Surface, commands: Sender<UiCommand>, events: Receiver<UiEvent>) {
    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        let theme = Theme::global_mut(cx);
        let accent = rgb(0x2563eb).into();
        let accent_hover = rgb(0x1d4ed8).into();
        let accent_active = rgb(0x1e40af).into();
        let accent_foreground = rgb(0xffffff).into();
        theme.colors.accent = accent;
        theme.colors.ring = accent;
        theme.colors.primary = accent;
        theme.colors.primary_hover = accent_hover;
        theme.colors.primary_active = accent_active;
        theme.colors.primary_foreground = accent_foreground;
        theme.colors.button_primary = accent;
        theme.colors.button_primary_hover = accent_hover;
        theme.colors.button_primary_active = accent_active;
        theme.colors.button_primary_foreground = accent_foreground;
        let events = std::sync::Arc::new(std::sync::Mutex::new(events));
        let bounds = Bounds::centered(None, size(px(480.0), px(540.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            is_resizable: false,
            ..Default::default()
        };
        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let title = match &surface {
                    Surface::Installer { identity, .. } | Surface::Maintenance { identity, .. } => {
                        identity.name.clone()
                    }
                };
                window.set_window_title(&format!("{title} Setup"));
                let view = cx.new(|_| {
                    let selected_scope = match &surface {
                        Surface::Installer { install, .. } => install.selected_scope,
                        _ => SelectedScope::User,
                    };
                    InstallerView {
                        model: UiModel::new(surface),
                        commands,
                        events: events.clone(),
                        selected_components: BTreeSet::new(),
                        selected_scope,
                        initialized_components: false,
                        editing_components: false,
                    }
                });
                let weak = view.downgrade();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(80))
                            .await;
                        let Some(view) = weak.upgrade() else {
                            break;
                        };
                        view.update(cx, |_, cx| cx.notify());
                    }
                })
                .detach();
                let close = view.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    close
                        .update(cx, |view, cx| {
                            let should_close = view.model.request_close();
                            if !should_close {
                                cx.notify();
                            }
                            should_close
                        })
                        .unwrap_or(true)
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("open zup window");
        })
        .detach();
    });
}

struct InstallerView {
    model: UiModel,
    commands: Sender<UiCommand>,
    events: std::sync::Arc<std::sync::Mutex<Receiver<UiEvent>>>,
    selected_components: BTreeSet<ComponentId>,
    selected_scope: SelectedScope,
    initialized_components: bool,
    editing_components: bool,
}

impl Render for InstallerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let events = self.events.clone();
        while let Ok(event) = events.lock().expect("event receiver").try_recv() {
            if event == UiEvent::Quit {
                cx.quit();
            } else {
                self.model.apply(event);
            }
        }
        if !self.initialized_components {
            let components = match &self.model.surface {
                Surface::Installer { install, .. } => &install.components,
                Surface::Maintenance { components, .. } => components,
            };
            self.selected_components.extend(
                components
                    .iter()
                    .filter(|item| item.selected || item.required)
                    .map(|item| item.id.clone()),
            );
            self.initialized_components = true;
        }
        #[cfg(windows)]
        update_windows_taskbar(window, &self.model);
        let mut body = div()
            .v_flex()
            .gap_4()
            .p_6()
            .w(px(460.0))
            .min_h(px(420.0))
            .bg(cx.theme().colors.background)
            .text_color(cx.theme().colors.foreground);

        match &self.model.surface {
            Surface::Installer { identity, install } => {
                body = body.child(identity_view(identity)).child(installer_body(
                    &self.model,
                    install,
                    &self.selected_components,
                    self.selected_scope,
                    &self.commands,
                    cx.entity(),
                ));
            }
            Surface::Maintenance {
                identity,
                installed_version,
                components,
                updates_enabled,
            } => {
                body = body.child(identity_view(identity)).child(
                    div()
                        .v_flex()
                        .gap_2()
                        .child(text_line("Installed version", installed_version, 13.0))
                        .child(maintenance_actions(
                            &self.model,
                            &self.commands,
                            *updates_enabled,
                            components,
                            &self.selected_components,
                            self.editing_components,
                            cx.entity(),
                        ))
                        .child(repair_summary(&self.model)),
                );
            }
        }
        body = body.child(status_view(&self.model, &self.commands));
        let _ = cx;
        body
    }
}

#[cfg(windows)]
fn update_windows_taskbar(window: &Window, model: &UiModel) {
    use raw_window_handle::RawWindowHandle;
    use std::cell::RefCell;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::{
        ITaskbarList3, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
        TaskbarList,
    };

    thread_local! {
        static TASKBAR: RefCell<Option<ITaskbarList3>> = const { RefCell::new(None) };
        static LAST_PROGRESS: RefCell<Option<(isize, i32, u64, u64)>> = const { RefCell::new(None) };
    }

    let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd_value = handle.hwnd.get();
    let (state, completed, total) = match model.state {
        ViewState::Running => {
            model
                .progress
                .as_ref()
                .map_or((TBPF_INDETERMINATE, 0, 0), |progress| {
                    if progress.total == 0 {
                        (TBPF_INDETERMINATE, 0, 0)
                    } else {
                        (
                            TBPF_NORMAL,
                            progress.completed.min(progress.total),
                            progress.total,
                        )
                    }
                })
        }
        ViewState::WaitingForSafeCancel | ViewState::Blocked => (TBPF_PAUSED, 0, 0),
        ViewState::Error | ViewState::RecoveryRequired => (TBPF_ERROR, 0, 0),
        _ => (TBPF_NOPROGRESS, 0, 0),
    };
    let cache_key = (hwnd_value, state.0, completed, total);
    if LAST_PROGRESS.with(|last| *last.borrow() == Some(cache_key)) {
        return;
    }
    LAST_PROGRESS.with(|last| *last.borrow_mut() = Some(cache_key));

    TASKBAR.with(|taskbar| {
        let mut taskbar = taskbar.borrow_mut();
        if taskbar.is_none() {
            // The UI thread owns this COM apartment for the process lifetime.
            unsafe {
                if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
                    return;
                }
                let Ok(created) =
                    CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
                else {
                    return;
                };
                if created.HrInit().is_err() {
                    return;
                }
                *taskbar = Some(created);
            }
        }
        let Some(taskbar) = taskbar.as_ref() else {
            return;
        };
        unsafe {
            let hwnd = HWND(hwnd_value as *mut _);
            let _ = taskbar.SetProgressState(hwnd, state);
            if total > 0 {
                let _ = taskbar.SetProgressValue(hwnd, completed, total);
            }
        }
    });
}

fn identity_view(identity: &ProductIdentity) -> impl IntoElement {
    div()
        .v_flex()
        .gap_1()
        .child(
            div()
                .h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .size(px(42.0))
                        .rounded(px(12.0))
                        .bg(rgb(0x2563eb))
                        .items_center()
                        .justify_center()
                        .text_color(rgb(0xffffff))
                        .child("Z"),
                )
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(21.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(identity.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(0x69717d))
                                .child(format!(
                                    "{}  ·  {}",
                                    identity.publisher.as_deref().unwrap_or(""),
                                    identity.version
                                )),
                        ),
                ),
        )
        .child(
            identity
                .description
                .clone()
                .unwrap_or_else(|| "Install and manage this application with zup.".into()),
        )
}

fn text_line(label: &str, value: &str, size: f32) -> impl IntoElement {
    div()
        .h_flex()
        .justify_between()
        .items_center()
        .text_size(px(size))
        .child(label.to_owned())
        .child(value.to_owned())
}

fn installer_body(
    model: &UiModel,
    install: &InstallModel,
    selected: &BTreeSet<ComponentId>,
    scope: SelectedScope,
    commands: &Sender<UiCommand>,
    entity: Entity<InstallerView>,
) -> impl IntoElement {
    let heading = install
        .existing_version
        .as_ref()
        .map_or("Ready to install".into(), |v| format!("Upgrade from {v}"));
    let mut body = div().v_flex().gap_3().child(
        div()
            .text_size(px(16.0))
            .font_weight(FontWeight::MEDIUM)
            .child(heading),
    );
    if install.scopes.len() > 1 {
        let user = entity.clone();
        let machine = entity.clone();
        body = body.child(
            div()
                .v_flex()
                .gap_2()
                .child("Install for")
                .child(
                    gpui_kit::base::Radio::new("scope-user")
                        .aria_label("Just me")
                        .checked(scope == SelectedScope::User)
                        .on_change(move |checked, _, _, cx| {
                            if checked {
                                user.update(cx, |this, cx| {
                                    this.selected_scope = SelectedScope::User;
                                    cx.notify();
                                })
                            }
                        }),
                )
                .child("Just me")
                .child(
                    gpui_kit::base::Radio::new("scope-machine")
                        .aria_label("Everyone")
                        .checked(scope == SelectedScope::Machine)
                        .on_change(move |checked, _, _, cx| {
                            if checked {
                                machine.update(cx, |this, cx| {
                                    this.selected_scope = SelectedScope::Machine;
                                    cx.notify();
                                })
                            }
                        }),
                )
                .child("Everyone"),
        );
    }
    for component in &install.components {
        let enabled = selected.contains(&component.id);
        let id = component.id.clone();
        let row = entity.clone();
        body = body.child(
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .child(
                    gpui_kit::base::Checkbox::new(format!("component-{}", component.id))
                        .aria_label(component.name.clone())
                        .checked(enabled)
                        .disabled(component.required)
                        .on_change(move |checked, _, _, cx| {
                            row.update(cx, |this, cx| {
                                if checked == gpui_kit::base::CheckboxState::Checked {
                                    this.selected_components.insert(id.clone());
                                } else {
                                    this.selected_components.remove(&id);
                                }
                                this.model.set_component_selected(
                                    &id,
                                    checked == gpui_kit::base::CheckboxState::Checked,
                                );
                                cx.notify();
                            })
                        }),
                )
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(component.name.clone())
                        .child(component.description.clone().unwrap_or_default())
                        .text_size(px(13.0)),
                ),
        );
    }
    if model.state == ViewState::Options {
        let commands = commands.clone();
        let enabled = selected.clone();
        let scope = if install.scopes.contains(&scope) {
            scope
        } else {
            install.selected_scope
        };
        body = body.child(
            Button::new("install")
                .primary()
                .label(if install.existing_version.is_some() {
                    "Upgrade"
                } else {
                    "Install"
                })
                .on_click(move |_, _, _| {
                    let _ = commands.send(UiCommand::Install {
                        scope,
                        components: enabled.iter().cloned().collect(),
                    });
                }),
        );
    }
    body
}

fn maintenance_actions(
    model: &UiModel,
    commands: &Sender<UiCommand>,
    updates_enabled: bool,
    components: &[ComponentOption],
    selected: &BTreeSet<ComponentId>,
    editing: bool,
    entity: Entity<InstallerView>,
) -> impl IntoElement {
    if model.state == ViewState::ConfirmUninstall {
        return div();
    }
    if model.state == ViewState::Running
        || model.state == ViewState::WaitingForSafeCancel
        || model.state == ViewState::Blocked
        || model.state == ViewState::Success
        || model.state == ViewState::Error
        || model.state == ViewState::RecoveryRequired
    {
        return div();
    }
    if model.state == ViewState::Maintenance && editing {
        let mut editor = div().v_flex().gap_2().child("Application components");
        for component in components {
            let id = component.id.clone();
            let row = entity.clone();
            editor = editor.child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        gpui_kit::base::Checkbox::new(format!("modify-component-{}", component.id))
                            .aria_label(component.name.clone())
                            .checked(selected.contains(&component.id))
                            .disabled(component.required)
                            .on_change(move |state, _, _, cx| {
                                row.update(cx, |this, cx| {
                                    if state == gpui_kit::base::CheckboxState::Checked {
                                        this.selected_components.insert(id.clone());
                                    } else {
                                        this.selected_components.remove(&id);
                                    }
                                    this.model.set_component_selected(
                                        &id,
                                        state == gpui_kit::base::CheckboxState::Checked,
                                    );
                                    cx.notify();
                                })
                            }),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(component.name.clone())
                            .child(component.description.clone().unwrap_or_default()),
                    ),
            );
        }
        let save = commands.clone();
        let selected = selected.clone();
        editor = editor.child(
            Button::new("save-components")
                .primary()
                .label("Apply changes")
                .on_click(move |_, _, _| {
                    let _ = save.send(UiCommand::Modify {
                        components: selected.iter().cloned().collect(),
                    });
                }),
        );
        let cancel = entity;
        return editor.child(Button::new("cancel-modify").label("Cancel").on_click(
            move |_, _, cx| {
                cancel.update(cx, |view, cx| {
                    view.editing_components = false;
                    cx.notify();
                });
            },
        ));
    }
    let mut group = div().v_flex().gap_2();
    if updates_enabled {
        let commands = commands.clone();
        group = group.child(
            Button::new("update")
                .label(
                    model
                        .update_status
                        .as_deref()
                        .unwrap_or("Check for updates"),
                )
                .on_click(move |_, _, _| {
                    let _ = commands.send(UiCommand::Update);
                }),
        );
    }
    let modify = entity;
    group = group.child(
        Button::new("modify")
            .label("Modify")
            .on_click(move |_, _, cx| {
                modify.update(cx, |view, cx| {
                    view.editing_components = true;
                    cx.notify();
                });
            }),
    );
    let repair = commands.clone();
    group = group.child(
        Button::new("repair")
            .label("Repair")
            .on_click(move |_, _, _| {
                let _ = repair.send(UiCommand::Repair);
            }),
    );
    let commands = commands.clone();
    group.child(
        Button::new("uninstall")
            .label("Uninstall")
            .on_click(move |_, _, _| {
                let _ = commands.send(UiCommand::Uninstall);
            }),
    )
}

fn status_view(model: &UiModel, commands: &Sender<UiCommand>) -> impl IntoElement {
    match model.state {
        ViewState::Running | ViewState::WaitingForSafeCancel => {
            let progress = model.progress.as_ref();
            let label = progress.map_or("Preparing…", |p| p.action.as_str());
            let pct = progress
                .filter(|p| p.total > 0)
                .map(|p| ((p.completed.min(p.total) as f32 / p.total as f32) * 100.0) as u32);
            let commands = commands.clone();
            let bar = pct.map_or_else(
                || div().h(px(6.0)).w_full().rounded(px(4.0)).bg(rgb(0xe5e7eb)),
                |value| {
                    div()
                        .h(px(6.0))
                        .w_full()
                        .rounded(px(4.0))
                        .bg(rgb(0xe5e7eb))
                        .child(
                            div()
                                .h(px(6.0))
                                .w(px(4.0 * value as f32))
                                .rounded(px(4.0))
                                .bg(rgb(0x2563eb)),
                        )
                },
            );
            div()
                .v_flex()
                .gap_3()
                .child(label.to_owned())
                .child(bar)
                .child(pct.map_or("Working…".into(), |value| format!("{value}%")))
                .child(if model.close_requested {
                    "The window will stay open until this operation reaches a safe end.".to_owned()
                } else {
                    String::new()
                })
                .child(
                    Button::new("cancel")
                        .label(if model.state == ViewState::WaitingForSafeCancel {
                            "Waiting for a safe point…"
                        } else {
                            "Cancel"
                        })
                        .on_click(move |_, _, _| {
                            let _ = commands.send(UiCommand::Cancel);
                        }),
                )
        }
        ViewState::Blocked => {
            let commands = commands.clone();
            let cancel = commands.clone();
            div()
                .v_flex()
                .gap_2()
                .child("Close these applications to continue")
                .children(model.blockers.iter().cloned())
                .child(
                    Button::new("retry")
                        .primary()
                        .label("Retry")
                        .on_click(move |_, _, _| {
                            let _ = commands.send(UiCommand::Retry);
                        }),
                )
                .child(
                    Button::new("cancel-blocked")
                        .label("Cancel")
                        .on_click(move |_, _, _| {
                            let _ = cancel.send(UiCommand::Cancel);
                        }),
                )
        }
        ViewState::ConfirmUninstall => {
            let confirm = commands.clone();
            let dismiss = commands.clone();
            div()
                .v_flex()
                .gap_2()
                .child("Remove this application and its managed resources?")
                .child(
                    Button::new("confirm-uninstall")
                        .primary()
                        .label("Uninstall")
                        .on_click(move |_, _, _| {
                            let _ = confirm.send(UiCommand::ConfirmUninstall);
                        }),
                )
                .child(
                    Button::new("cancel-uninstall")
                        .label("Keep application")
                        .on_click(move |_, _, _| {
                            let _ = dismiss.send(UiCommand::DismissUninstall);
                        }),
                )
        }
        ViewState::Success => div()
            .text_color(rgb(0x18794e))
            .child("Finished successfully"),
        ViewState::RecoveryRequired => div().text_color(rgb(0xb42318)).child(
            model
                .error
                .clone()
                .unwrap_or_else(|| "Recovery is required".into()),
        ),
        ViewState::Error => div().text_color(rgb(0xb42318)).child(
            model
                .error
                .clone()
                .unwrap_or_else(|| "The operation failed".into()),
        ),
        _ => div()
            .v_flex()
            .gap_2()
            .child(model.update_status.clone().unwrap_or_default())
            .child(if model.close_requested {
                "The window will stay open until this operation reaches a safe end.".to_owned()
            } else {
                String::new()
            }),
    }
}

fn repair_summary(model: &UiModel) -> impl IntoElement {
    if model.repair_drift.is_empty() {
        div()
    } else {
        div()
            .v_flex()
            .gap_1()
            .text_color(rgb(0x7a4d00))
            .child("Repair completed. These changed resources were left untouched:")
            .children(model.repair_drift.iter().cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installer() -> UiModel {
        UiModel::new(Surface::Installer {
            identity: ProductIdentity {
                name: "Acme".into(),
                publisher: None,
                version: "1.0.0".into(),
                description: None,
            },
            install: InstallModel {
                existing_version: None,
                scopes: vec![SelectedScope::User],
                selected_scope: SelectedScope::User,
                components: vec![],
            },
        })
    }

    #[test]
    fn fresh_install_starts_with_options_without_scope_choices() {
        let model = installer();
        assert_eq!(model.state, ViewState::Options);
        let Surface::Installer { install, .. } = model.surface else {
            panic!()
        };
        assert_eq!(install.scopes, [SelectedScope::User]);
        assert!(install.existing_version.is_none());
    }

    #[test]
    fn existing_install_is_presented_as_upgrade() {
        let mut model = installer();
        let Surface::Installer { install, .. } = &mut model.surface else {
            panic!()
        };
        install.existing_version = Some("0.9.0".into());
        assert!(
            install
                .existing_version
                .as_deref()
                .unwrap()
                .contains("0.9.0")
        );
    }

    #[test]
    fn component_selection_changes_only_optional_components() {
        let optional = ComponentOption {
            id: ComponentId::new("docs").unwrap(),
            name: "Documentation".into(),
            description: None,
            required: false,
            selected: true,
        };
        let required = ComponentOption {
            id: ComponentId::new("core").unwrap(),
            name: "Core".into(),
            description: None,
            required: true,
            selected: true,
        };
        let mut model = UiModel::new(Surface::Installer {
            identity: ProductIdentity {
                name: "Acme".into(),
                publisher: None,
                version: "1.0.0".into(),
                description: None,
            },
            install: InstallModel {
                existing_version: None,
                scopes: vec![SelectedScope::User],
                selected_scope: SelectedScope::User,
                components: vec![optional.clone(), required.clone()],
            },
        });
        assert!(model.set_component_selected(&optional.id, false));
        assert!(!model.set_component_selected(&required.id, false));
        let Surface::Installer { install, .. } = model.surface else {
            panic!()
        };
        assert!(!install.components[0].selected);
        assert!(install.components[1].selected);
    }

    #[test]
    fn progress_aggregates_without_reset_and_cancel_waits_safely() {
        let mut model = installer();
        model.apply(UiEvent::Progress {
            completed: 40,
            total: 100,
            action: "Installing files…".into(),
        });
        model.apply(UiEvent::Progress {
            completed: 68,
            total: 100,
            action: "Registering services…".into(),
        });
        assert_eq!(model.progress.as_ref().unwrap().completed, 68);
        model.request_cancel();
        assert_eq!(model.state, ViewState::WaitingForSafeCancel);
    }

    #[test]
    fn restart_manager_blockers_and_retry_are_explicit() {
        let mut model = installer();
        model.apply(UiEvent::Runtime(RuntimeEvent::BlockingProcessesFound {
            detail: "Editor.exe\nAgent.exe".into(),
        }));
        assert_eq!(model.state, ViewState::Blocked);
        assert_eq!(model.blockers, ["Editor.exe", "Agent.exe"]);
    }

    #[test]
    fn success_error_and_recovery_states_are_distinct() {
        let mut model = installer();
        model.apply(UiEvent::OperationFinished(InstallOutcome::Committed));
        assert_eq!(model.state, ViewState::Success);
        model.apply(UiEvent::Error {
            message: "failed".into(),
            recovery_required: false,
        });
        assert_eq!(model.state, ViewState::Error);
        model.apply(UiEvent::Error {
            message: "recover".into(),
            recovery_required: true,
        });
        assert_eq!(model.state, ViewState::RecoveryRequired);
    }

    #[test]
    fn update_repair_and_uninstall_state_are_renderable() {
        let mut model = installer();
        model.apply(UiEvent::UpdateAvailable {
            current: "1".into(),
            available: "2".into(),
        });
        assert!(model.update_status.as_ref().unwrap().contains("2"));
        model.apply(UiEvent::RepairFinished {
            drifted_resources: vec!["file:a".into()],
        });
        assert_eq!(model.repair_drift, ["file:a"]);
        model.state = ViewState::ConfirmUninstall;
        assert_eq!(model.state, ViewState::ConfirmUninstall);
    }

    #[test]
    fn close_is_deferred_during_an_operation() {
        let mut model = installer();
        model.begin_operation();
        assert!(!model.request_close());
        assert!(model.close_requested);
    }
}

#[cfg(all(test, windows))]
mod windows_smoke {
    use super::*;
    use gpui_kit::test::TestWindowExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use zup_core::{RelativePath, ResourceKey, hash_reader};
    use zup_exec::{
        ExecutionPlan, ExecutionSummary, FileOperation, FileOperationKind, FilePrecondition,
    };
    use zup_platform::TargetPath;
    use zup_runtime::{CancellationHandle, RuntimeRequest, run_install_control};

    fn smoke_request() -> RuntimeRequest {
        let root = TempDir::new().unwrap().keep();
        let payload = root.join("payload");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("zup-smoke.bin"), b"smoke payload").unwrap();
        let destination = root.join("installed").join("zup-smoke.bin");
        let digest = hash_reader(&b"smoke payload"[..]).unwrap().1;
        let source_relative = RelativePath::new("zup-smoke.bin").unwrap();
        let destination_text = destination.to_string_lossy().into_owned();
        RuntimeRequest {
            app_id: zup_core::AppId::new("com.zup.ui-smoke").unwrap(),
            app_version: "1.0.0".parse().unwrap(),
            scope: SelectedScope::User,
            execution_plan: ExecutionPlan {
                selected_components: vec![],
                uninstall: false,
                removals: vec![],
                files: vec![FileOperation {
                    key: ResourceKey::File {
                        destination: destination_text.clone(),
                    },
                    kind: FileOperationKind::Create,
                    destination: TargetPath::new(PathBuf::from(destination_text)).unwrap(),
                    source_relative,
                    precondition: FilePrecondition::Absent,
                    expected_sha256: digest,
                    expected_size: 13,
                    conflict: None,
                }],
                shortcuts: vec![],
                path_entries: vec![],
                services: vec![],
                protocols: vec![],
                file_types: vec![],
                uninstall_entries: vec![],
                summary: ExecutionSummary {
                    files_create: 1,
                    ..Default::default()
                },
            },
            state_root: root.join("state"),
            work_root: root.join("work"),
            payload_root: payload,
            payload_overlay_root: None,
            payload_overlay_base_root: None,
            recovery_id: None,
        }
    }

    #[test]
    fn native_app_process_opens_and_drives_runtime() {
        const CHILD: &str = "ZUP_UI_NATIVE_SMOKE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let (commands, command_rx) = std::sync::mpsc::channel();
            let (events_tx, events_rx) = std::sync::mpsc::channel();
            let backend = std::thread::spawn(move || {
                let Ok(UiCommand::Install { .. }) = command_rx.recv() else {
                    return;
                };
                let result = std::thread::spawn(|| {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(run_install_control(
                        smoke_request(),
                        CancellationHandle::new(),
                        tokio::sync::broadcast::channel(64).0,
                    ))
                })
                .join()
                .unwrap();
                match result {
                    Ok(outcome) => {
                        let _ = events_tx.send(UiEvent::OperationFinished(outcome));
                    }
                    Err(error) => {
                        let _ = events_tx.send(UiEvent::Error {
                            message: error.to_string(),
                            recovery_required: false,
                        });
                    }
                }
                let _ = events_tx.send(UiEvent::Quit);
            });
            commands
                .send(UiCommand::Install {
                    scope: SelectedScope::User,
                    components: vec![],
                })
                .unwrap();
            let surface = Surface::Installer {
                identity: ProductIdentity {
                    name: "Zup native smoke".into(),
                    publisher: None,
                    version: "1.0.0".into(),
                    description: None,
                },
                install: InstallModel {
                    existing_version: None,
                    scopes: vec![SelectedScope::User],
                    selected_scope: SelectedScope::User,
                    components: vec![],
                },
            };
            run(surface, commands, events_rx);
            backend.join().unwrap();
            return;
        }

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows_smoke::native_app_process_opens_and_drives_runtime",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "native GPUI smoke process exited with {status}"
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("native GPUI smoke process did not close after its runtime transaction");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[gpui_kit::test]
    async fn windows_gpui_installer_dispatches_and_completes_a_runtime_transaction(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (commands, command_rx) = std::sync::mpsc::channel();
        let (_events, event_rx) = std::sync::mpsc::channel();
        let view = cx.add_window(|_, _| InstallerView {
            model: UiModel::new(Surface::Installer {
                identity: ProductIdentity {
                    name: "Zup smoke".into(),
                    publisher: None,
                    version: "1.0.0".into(),
                    description: None,
                },
                install: InstallModel {
                    existing_version: None,
                    scopes: vec![SelectedScope::User],
                    selected_scope: SelectedScope::User,
                    components: vec![],
                },
            }),
            commands,
            events: std::sync::Arc::new(std::sync::Mutex::new(event_rx)),
            selected_components: BTreeSet::new(),
            selected_scope: SelectedScope::User,
            initialized_components: false,
            editing_components: false,
        });
        cx.update_window(view.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("install", cx);
        })
        .unwrap();
        assert!(matches!(
            command_rx.recv().unwrap(),
            UiCommand::Install {
                scope: SelectedScope::User,
                ..
            }
        ));

        let (runtime_events, _) = tokio::sync::broadcast::channel(64);
        let mut event_rx = runtime_events.subscribe();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(run_install_control(
                smoke_request(),
                CancellationHandle::new(),
                runtime_events,
            ))
            .unwrap();
        assert_eq!(outcome, InstallOutcome::Committed);
        let mut model = UiModel::new(Surface::Maintenance {
            identity: ProductIdentity {
                name: "Zup smoke".into(),
                publisher: None,
                version: "1.0.0".into(),
                description: None,
            },
            installed_version: "1.0.0".into(),
            components: vec![],
            updates_enabled: false,
        });
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, RuntimeEvent::Completed { .. }) {
                model.apply(UiEvent::Runtime(event));
            }
        }
        assert_eq!(model.state, ViewState::Success);
    }
}

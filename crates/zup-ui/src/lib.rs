//! Native installer and maintenance frontend.
//!
//! This crate owns presentation state only. The host application translates
//! [`UiCommand`] values into planner/runtime requests and returns runtime events.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender};

use gpui_kit::base::{Disableable, StyledExt};
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{ActiveTheme, Root, Theme, ThemeMode};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, FontWeight, IntoElement, ParentElement, Render, Styled,
    Window, WindowBounds, WindowOptions, application, assets, div, px, rgb, size,
};
use zup_core::{ComponentId, SelectedScope, UiBranding, UiTheme};
use zup_presentation::{
    DiagnosticPresentation, InstallationHealth, OperationPhase, PlanPreview, RequirementStatus,
    UpdatePresentation,
};
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
        scope: SelectedScope,
        install_directory: Option<String>,
        health: InstallationHealth,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallModel {
    pub existing_version: Option<String>,
    pub scopes: Vec<SelectedScope>,
    pub selected_scope: SelectedScope,
    pub components: Vec<ComponentOption>,
    pub install_directory: Option<String>,
    pub allow_directory_override: bool,
    pub estimated_bytes: u64,
    pub requires_authorization: bool,
    pub preview: Option<PlanPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    Install {
        scope: SelectedScope,
        components: Vec<ComponentId>,
        install_directory: Option<String>,
    },
    Preview {
        scope: SelectedScope,
        components: Vec<ComponentId>,
        install_directory: Option<String>,
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
    OpenLog,
    CopyDiagnostics,
    Close,
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
    PlanReady(PlanPreview),
    UpdateStatus(UpdatePresentation),
    LogPath(String),
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
    pub phase: OperationPhase,
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
    pub diagnostic: Option<DiagnosticPresentation>,
    pub update: Option<UpdatePresentation>,
    pub log_path: Option<String>,
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
            diagnostic: None,
            update: None,
            log_path: None,
            close_requested: false,
        }
    }

    pub fn begin_operation(&mut self) {
        self.state = ViewState::Running;
        self.progress = Some(ProgressModel {
            completed: 0,
            total: 0,
            action: "Preparing…".into(),
            phase: OperationPhase::Prepare,
        });
        self.error = None;
        self.diagnostic = None;
        self.blockers.clear();
        self.close_requested = false;
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

    pub fn has_customization(&self) -> bool {
        match &self.surface {
            Surface::Installer { install, .. } => {
                install.scopes.len() > 1
                    || install
                        .components
                        .iter()
                        .any(|component| !component.required)
                    || install.allow_directory_override
            }
            Surface::Maintenance { components, .. } => {
                components.iter().any(|component| !component.required)
            }
        }
    }

    pub fn set_install_directory(&mut self, directory: Option<String>) -> bool {
        let Surface::Installer { install, .. } = &mut self.surface else {
            return false;
        };
        if !install.allow_directory_override && directory.is_some() {
            return false;
        }
        install.install_directory = directory;
        true
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
            UiEvent::DismissUninstall => {
                self.state = if matches!(self.surface, Surface::Installer { .. }) {
                    ViewState::Options
                } else {
                    ViewState::Maintenance
                }
            }
            UiEvent::CancellationWaiting => self.request_cancel(),
            UiEvent::Progress {
                completed,
                total,
                action,
            } => {
                let waiting_for_cancel = self.state == ViewState::WaitingForSafeCancel;
                self.progress = Some(ProgressModel {
                    completed,
                    total,
                    phase: OperationPhase::from_action(&action),
                    action,
                });
                self.state = if waiting_for_cancel {
                    ViewState::WaitingForSafeCancel
                } else {
                    ViewState::Running
                };
            }
            UiEvent::UpdateAvailable { current, available } => {
                self.update_status = Some(format!("Update available · {current} → {available}"));
                self.update = Some(UpdatePresentation {
                    channel: None,
                    state: "Update available".into(),
                    current: Some(current),
                    available: Some(available),
                });
            }
            UiEvent::UpToDate { current } => {
                self.update_status = Some(format!("Up to date · {current}"));
                self.update = Some(UpdatePresentation {
                    channel: None,
                    state: "Up to date".into(),
                    current: Some(current),
                    available: None,
                });
            }
            UiEvent::PlanReady(preview) => {
                if let Surface::Installer { install, .. } = &mut self.surface {
                    install.preview = Some(preview);
                }
            }
            UiEvent::UpdateStatus(update) => {
                self.update_status = Some(update.state.clone());
                self.update = Some(update);
            }
            UiEvent::LogPath(path) => {
                self.log_path = Some(path);
            }
            UiEvent::RepairFinished { drifted_resources } => {
                self.repair_drift = drifted_resources.clone();
                if let Surface::Maintenance { health, .. } = &mut self.surface {
                    health.drift_count = drifted_resources.len();
                    health.state = if drifted_resources.is_empty() {
                        "Healthy"
                    } else {
                        "Changed"
                    }
                    .into();
                    health.summary = if drifted_resources.is_empty() {
                        "All managed files match".into()
                    } else {
                        "Some managed files were left untouched".into()
                    };
                }
            }
            UiEvent::OperationFinished(outcome) => match outcome {
                InstallOutcome::RebootRequired {
                    prerequisite_id,
                    exit_code,
                } => {
                    self.diagnostic = Some(DiagnosticPresentation::from_message(
                        &format!("restart required by {prerequisite_id} ({exit_code})"),
                        false,
                    ));
                    self.error = Some(format!(
                        "Restart required by {prerequisite_id} before continuing ({exit_code})"
                    ));
                    self.state = ViewState::Error;
                }
                InstallOutcome::Committed => {
                    self.state = ViewState::Success;
                    self.close_requested = false;
                }
                InstallOutcome::RolledBack | InstallOutcome::Cancelled => {
                    self.state = if matches!(self.surface, Surface::Installer { .. }) {
                        ViewState::Options
                    } else {
                        ViewState::Maintenance
                    };
                    self.progress = None;
                    self.error = None;
                    self.diagnostic = None;
                    self.close_requested = false;
                }
                InstallOutcome::RecoveryRequired => {
                    self.diagnostic = Some(DiagnosticPresentation::from_message(
                        "recovery required",
                        true,
                    ));
                    self.state = ViewState::RecoveryRequired;
                }
                InstallOutcome::Failed(message) => {
                    self.diagnostic = Some(DiagnosticPresentation::from_message(&message, false));
                    self.error = Some(message);
                    self.state = ViewState::Error;
                }
            },
            UiEvent::Error {
                message,
                recovery_required,
            } => {
                self.diagnostic = Some(DiagnosticPresentation::from_message(
                    &message,
                    recovery_required,
                ));
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
            RuntimeEvent::WaitingForAuthorization => self.action("Waiting for approval…"),
            RuntimeEvent::WorkerConnected => self.action("Starting…"),
            RuntimeEvent::PrerequisiteCheck {
                name, satisfied, ..
            } => {
                let label = if satisfied {
                    format!("✓ {name}")
                } else {
                    format!("↓ {name}")
                };
                self.action(&label);
            }
            RuntimeEvent::PrerequisiteDownload {
                completed, total, ..
            } => {
                self.progress = Some(ProgressModel {
                    completed,
                    total: total.unwrap_or(0),
                    phase: OperationPhase::Download,
                    action: "Downloading required component…".into(),
                });
                self.state = ViewState::Running;
            }
            RuntimeEvent::PrerequisiteInstall { name, .. } => {
                self.action(&format!("Installing {name}…"));
            }
            RuntimeEvent::RebootRequired { .. } => {
                self.diagnostic = Some(DiagnosticPresentation::from_message(
                    "restart required before continuing",
                    false,
                ));
                self.error = Some("Restart Windows, then run setup again.".into());
                self.state = ViewState::Error;
            }
            RuntimeEvent::PreflightStarted => self.action("Checking for open applications…"),
            RuntimeEvent::ResourceBlocked { detail, .. } => {
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
                    phase: OperationPhase::from_action(&action),
                    action,
                });
                self.state = if waiting_for_cancel {
                    ViewState::WaitingForSafeCancel
                } else {
                    ViewState::Running
                };
            }
            RuntimeEvent::RollingBack => self.action("Restoring the previous state…"),
            RuntimeEvent::Completed { outcome } => {
                self.state = match outcome.as_str() {
                    "reboot_required" => {
                        self.error = Some("Restart Windows, then run setup again.".into());
                        ViewState::Error
                    }
                    "recovery_required" => ViewState::RecoveryRequired,
                    _ => ViewState::Success,
                };
                self.close_requested = false;
            }
            RuntimeEvent::LogPath { path } => {
                self.log_path = Some(path);
            }
            RuntimeEvent::Failed { kind, message } => {
                let recovery_required = kind == "recovery_required";
                self.diagnostic = Some(DiagnosticPresentation::from_message(
                    &message,
                    recovery_required,
                ));
                self.error = Some(message);
                self.state = if recovery_required {
                    ViewState::RecoveryRequired
                } else {
                    ViewState::Error
                };
            }
            RuntimeEvent::StateChanged { state } => match state {
                RuntimeState::Preparing => self.action("Preparing…"),
                RuntimeState::WaitingForAuthorization => self.action("Waiting for approval…"),
                RuntimeState::CheckingPrerequisites => self.action("Checking requirements…"),
                RuntimeState::InstallingPrerequisites => {
                    self.action("Installing required components…")
                }
                RuntimeState::RebootRequired => {
                    self.error = Some("Restart Windows, then run setup again.".into());
                    self.state = ViewState::Error;
                }
                RuntimeState::ConnectingWorker => self.action("Starting…"),
                RuntimeState::Executing => self.action("Installing files…"),
                RuntimeState::RollingBack => self.action("Restoring the previous state…"),
                RuntimeState::Completed => self.state = ViewState::Success,
                RuntimeState::Cancelled => {
                    self.state = if matches!(self.surface, Surface::Installer { .. }) {
                        ViewState::Options
                    } else {
                        ViewState::Maintenance
                    };
                }
                RuntimeState::Failed => self.state = ViewState::Error,
            },
        }
    }

    fn action(&mut self, action: &str) {
        let waiting_for_cancel = self.state == ViewState::WaitingForSafeCancel;
        self.state = if waiting_for_cancel {
            ViewState::WaitingForSafeCancel
        } else {
            ViewState::Running
        };
        self.progress
            .get_or_insert(ProgressModel {
                completed: 0,
                total: 0,
                action: action.into(),
                phase: OperationPhase::from_action(action),
            })
            .action = action.into();
        if let Some(progress) = &mut self.progress {
            progress.phase = OperationPhase::from_action(action);
        }
    }
}

fn friendly_action(id: &str) -> String {
    let id = id.to_ascii_lowercase();
    if id.contains("service") {
        "Registering services…".into()
    } else if id.contains("launcher") {
        "Updating launchers…".into()
    } else if id.contains("file") {
        "Installing files…".into()
    } else {
        "Finishing…".into()
    }
}

pub fn run(surface: Surface, commands: Sender<UiCommand>, events: Receiver<UiEvent>) {
    run_with_branding(surface, commands, events, None)
}

fn parse_accent(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.strip_prefix('#'))
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .unwrap_or(0x2563eb)
}

pub fn run_with_branding(
    surface: Surface,
    commands: Sender<UiCommand>,
    events: Receiver<UiEvent>,
    branding: Option<UiBranding>,
) {
    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        match branding
            .as_ref()
            .map(|branding| branding.theme)
            .unwrap_or(UiTheme::System)
        {
            UiTheme::System => Theme::sync_system_appearance(None, cx),
            UiTheme::Light => Theme::change(ThemeMode::Light, None, cx),
            UiTheme::Dark => Theme::change(ThemeMode::Dark, None, cx),
        }
        let theme = Theme::global_mut(cx);
        let accent_value = parse_accent(
            branding
                .as_ref()
                .and_then(|branding| branding.accent.as_deref()),
        );
        let accent = rgb(accent_value).into();
        let accent_hover = rgb(accent_value).into();
        let accent_active = rgb(accent_value).into();
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
        theme.radius = px(10.0);
        theme.radius_lg = px(16.0);
        theme.focus_ring = true;
        Theme::sync_base(cx);
        let events = std::sync::Arc::new(std::sync::Mutex::new(events));
        let bounds = Bounds::centered(None, size(px(640.0), px(620.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            is_resizable: true,
            ..Default::default()
        };
        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let (title, suffix) = match &surface {
                    Surface::Installer { identity, .. } => (identity.name.clone(), "Setup"),
                    Surface::Maintenance { identity, .. } => (identity.name.clone(), "Maintenance"),
                };
                window.set_window_title(&format!("{title} {suffix}"));
                let default_directory = match &surface {
                    Surface::Installer { install, .. } => {
                        install.install_directory.clone().unwrap_or_default()
                    }
                    Surface::Maintenance {
                        install_directory, ..
                    } => install_directory.clone().unwrap_or_default(),
                };
                let install_directory_input =
                    cx.new(|cx| InputState::new(window, cx).default_value(default_directory));
                let view = cx.new(|_| {
                    let selected_scope = match &surface {
                        Surface::Installer { install, .. } => install.selected_scope,
                        _ => SelectedScope::User,
                    };
                    let selected_install_directory = match &surface {
                        Surface::Installer { install, .. } => install.install_directory.clone(),
                        Surface::Maintenance {
                            install_directory, ..
                        } => install_directory.clone(),
                    };
                    InstallerView {
                        model: UiModel::new(surface),
                        commands,
                        events: events.clone(),
                        selected_components: BTreeSet::new(),
                        selected_scope,
                        selected_install_directory,
                        install_directory_input: Some(install_directory_input),
                        initialized_components: false,
                        editing_components: false,
                        customizing: false,
                        showing_changes: false,
                        details_open: false,
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
    selected_install_directory: Option<String>,
    install_directory_input: Option<Entity<InputState>>,
    initialized_components: bool,
    editing_components: bool,
    customizing: bool,
    showing_changes: bool,
    details_open: bool,
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
        let current_install_directory = self
            .install_directory_input
            .as_ref()
            .and_then(|state| {
                let value = state.read(cx).value().to_string();
                (!value.trim().is_empty()).then_some(value)
            })
            .or_else(|| self.selected_install_directory.clone());
        let mut body = div()
            .v_flex()
            .gap_5()
            .p_6()
            .w_full()
            .max_w(px(720.0))
            .min_h(px(420.0))
            .overflow_y_scrollbar()
            .bg(cx.theme().colors.background)
            .text_color(cx.theme().colors.foreground);

        match &self.model.surface {
            Surface::Installer { identity, install } => {
                body = body
                    .child(identity_view(identity, Theme::global(cx)))
                    .child(installer_body(
                        &self.model,
                        install,
                        InstallerRenderOptions {
                            selected: &self.selected_components,
                            scope: self.selected_scope,
                            selected_install_directory: current_install_directory.clone(),
                            install_directory_input: self.install_directory_input.clone(),
                            customizing: self.customizing,
                            showing_changes: self.showing_changes,
                            theme: Theme::global(cx),
                            commands: &self.commands,
                            entity: cx.entity(),
                        },
                    ));
            }
            Surface::Maintenance {
                identity,
                installed_version,
                components,
                updates_enabled,
                scope,
                install_directory,
                health,
            } => {
                body = body
                    .child(identity_view(identity, Theme::global(cx)))
                    .child(
                        div()
                            .v_flex()
                            .gap_4()
                            .child(maintenance_summary(
                                installed_version,
                                *scope,
                                install_directory.as_deref(),
                                components.len(),
                                health,
                            ))
                            .child(maintenance_actions(
                                &self.model,
                                &self.commands,
                                *updates_enabled,
                                components,
                                &self.selected_components,
                                self.editing_components,
                                cx.entity(),
                            ))
                            .child(repair_summary(&self.model, Theme::global(cx))),
                    );
            }
        }
        body = body.child(status_view(
            &self.model,
            &self.commands,
            self.details_open,
            Theme::global(cx),
            cx.entity(),
        ));
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

fn identity_view(identity: &ProductIdentity, theme: &Theme) -> impl IntoElement {
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
                        .bg(theme.colors.accent)
                        .items_center()
                        .justify_center()
                        .text_color(theme.colors.accent_foreground)
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
                                .text_color(theme.colors.muted_foreground)
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

struct InstallerRenderOptions<'a> {
    selected: &'a BTreeSet<ComponentId>,
    scope: SelectedScope,
    selected_install_directory: Option<String>,
    install_directory_input: Option<Entity<InputState>>,
    customizing: bool,
    showing_changes: bool,
    theme: &'a Theme,
    commands: &'a Sender<UiCommand>,
    entity: Entity<InstallerView>,
}

fn installer_body(
    model: &UiModel,
    install: &InstallModel,
    options: InstallerRenderOptions<'_>,
) -> impl IntoElement {
    let InstallerRenderOptions {
        selected,
        scope,
        selected_install_directory,
        install_directory_input,
        customizing,
        showing_changes,
        theme,
        commands,
        entity,
    } = options;
    let heading = install
        .existing_version
        .as_ref()
        .map_or("Install".into(), |version| {
            format!("Upgrade from {version}")
        });
    let location = selected_install_directory
        .or_else(|| install.install_directory.clone())
        .unwrap_or_else(|| "The default location".into());
    let summary = div()
        .v_flex()
        .gap_2()
        .p_4()
        .rounded(theme.radius_lg)
        .bg(theme.colors.popover)
        .child(
            div()
                .h_flex()
                .justify_between()
                .child("Install to")
                .child(location.clone()),
        )
        .child(
            div()
                .h_flex()
                .justify_between()
                .text_color(theme.colors.muted_foreground)
                .child("Download size")
                .child(zup_presentation::format_bytes(install.estimated_bytes)),
        )
        .child(
            div()
                .h_flex()
                .justify_between()
                .text_color(theme.colors.muted_foreground)
                .child("Permissions")
                .child(if install.requires_authorization {
                    "Administrator approval required"
                } else {
                    "Current user"
                }),
        );
    let mut body = div()
        .v_flex()
        .gap_4()
        .child(
            div()
                .text_size(px(18.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(heading),
        )
        .child(summary);

    let has_choices = install.scopes.len() > 1
        || install
            .components
            .iter()
            .any(|component| !component.required)
        || install.allow_directory_override;
    if has_choices {
        let customize = entity.clone();
        body = body.child(
            Button::new("customize")
                .label(if customizing {
                    "Hide customization"
                } else {
                    "Customize"
                })
                .on_click(move |_, _, cx| {
                    customize.update(cx, |view, cx| {
                        view.customizing = !view.customizing;
                        cx.notify();
                    })
                }),
        );
    }

    if customizing {
        if install.scopes.len() > 1 {
            let scope_entity = entity.clone();
            let scope_entity_machine = scope_entity.clone();
            let selected_index = if scope == SelectedScope::Machine {
                1
            } else {
                0
            };
            body = body.child(
                div().v_flex().gap_2().child("Install for").child(
                    RadioGroup::new("install-scope")
                        .selected_index(Some(selected_index))
                        .child(
                            Radio::new("scope-user")
                                .label("Just me")
                                .checked(scope == SelectedScope::User)
                                .on_change(move |checked, _, cx| {
                                    if *checked {
                                        scope_entity.update(cx, |view, cx| {
                                            view.selected_scope = SelectedScope::User;
                                            cx.notify();
                                        })
                                    }
                                }),
                        )
                        .child(
                            Radio::new("scope-machine")
                                .label("Everyone")
                                .checked(scope == SelectedScope::Machine)
                                .on_change(move |checked, _, cx| {
                                    if *checked {
                                        scope_entity_machine.update(cx, |view, cx| {
                                            view.selected_scope = SelectedScope::Machine;
                                            cx.notify();
                                        })
                                    }
                                }),
                        ),
                ),
            );
        }
        if install.allow_directory_override {
            body = body.child(
                div()
                    .v_flex()
                    .gap_2()
                    .child("Install directory")
                    .child(
                        install_directory_input
                            .as_ref()
                            .map(|state| Input::new(state).aria_label("Install directory").into_any_element())
                            .unwrap_or_else(|| div().child(location.clone()).into_any_element()),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.colors.muted_foreground)
                            .child("The author allows this application to be moved before installation."),
                    ),
            );
        }
        if !install.components.is_empty() {
            let mut components = div().v_flex().gap_2().child("Application components");
            for component in &install.components {
                let enabled = selected.contains(&component.id);
                let id = component.id.clone();
                let row = entity.clone();
                let label = if component.required {
                    format!("{} · Required", component.name)
                } else {
                    component.name.clone()
                };
                let control = Checkbox::new(format!("component-{}", component.id))
                    .label(label)
                    .accessibility_label(if component.required {
                        format!("{} (required)", component.name)
                    } else {
                        component.name.clone()
                    })
                    .checked(enabled)
                    .disabled(component.required)
                    .on_change(move |checked, _, cx| {
                        row.update(cx, |view, cx| {
                            if *checked {
                                view.selected_components.insert(id.clone());
                            } else {
                                view.selected_components.remove(&id);
                            }
                            view.model.set_component_selected(&id, *checked);
                            cx.notify();
                        })
                    });
                components = components.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(control)
                        .child(component.description.clone().unwrap_or_default()),
                );
            }
            body = body.child(components);
        }
    }

    if model.state == ViewState::Options {
        let preview_entity = entity.clone();
        let preview_commands = commands.clone();
        let preview_selected = selected.clone();
        let preview_input = install_directory_input.clone();
        let preview_scope = scope;
        body = body.child(
            Button::new("what-will-change")
                .label(if showing_changes {
                    "Hide changes"
                } else {
                    "What will change"
                })
                .on_click(move |_, _, cx| {
                    let directory = preview_input
                        .as_ref()
                        .map(|state| state.read(cx).value().to_string())
                        .filter(|value| !value.trim().is_empty());
                    let _ = preview_commands.send(UiCommand::Preview {
                        scope: preview_scope,
                        components: preview_selected.iter().cloned().collect(),
                        install_directory: directory,
                    });
                    preview_entity.update(cx, |view, cx| {
                        view.showing_changes = true;
                        cx.notify();
                    });
                }),
        );
        if showing_changes {
            body = body.child(match &install.preview {
                Some(preview) => change_preview(preview, theme).into_any_element(),
                None => div()
                    .p_4()
                    .rounded(theme.radius_lg)
                    .bg(theme.colors.muted)
                    .child("Preparing the change preview…")
                    .into_any_element(),
            });
        }
    }

    if model.state == ViewState::Options {
        let commands = commands.clone();
        let enabled = selected.clone();
        let selected_scope = if install.scopes.contains(&scope) {
            scope
        } else {
            install.selected_scope
        };
        let input = install_directory_input.clone();
        body = body.child(
            Button::new("install")
                .primary()
                .label(if install.existing_version.is_some() {
                    "Upgrade"
                } else {
                    "Install"
                })
                .on_click(move |_, _, cx| {
                    let directory = input
                        .as_ref()
                        .map(|state| state.read(cx).value().to_string())
                        .filter(|value| !value.trim().is_empty());
                    let _ = commands.send(UiCommand::Install {
                        scope: selected_scope,
                        components: enabled.iter().cloned().collect(),
                        install_directory: directory,
                    });
                }),
        );
    }
    body
}

fn change_preview(preview: &PlanPreview, theme: &Theme) -> impl IntoElement {
    let mut content = div()
        .v_flex()
        .gap_3()
        .p_4()
        .rounded(theme.radius_lg)
        .bg(theme.colors.muted);
    if !preview.requirements.is_empty() {
        content = content.child(
            div()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Requirements"),
                )
                .children(preview.requirements.iter().map(|requirement| {
                    let marker = match requirement.status {
                        RequirementStatus::Satisfied => "✓",
                        RequirementStatus::Missing => "+",
                        RequirementStatus::Unknown => "?",
                    };
                    div()
                        .h_flex()
                        .justify_between()
                        .gap_3()
                        .text_size(px(13.0))
                        .child(format!("{marker}  {}", requirement.name))
                        .child(if requirement.shared {
                            "Shared system dependency".to_owned()
                        } else {
                            String::new()
                        })
                }))
                .child(
                    div()
                        .text_size(px(11.0))
                        .child("Shared requirements are not removed with this application."),
                ),
        );
    }
    for group in &preview.groups {
        content = content.child(
            div()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(group.title.clone()),
                )
                .children(group.changes.iter().map(|change| {
                    div()
                        .h_flex()
                        .justify_between()
                        .gap_3()
                        .text_size(px(13.0))
                        .child(format!("{}  {}", change.kind.label(), change.label))
                        .child(change.location.clone().unwrap_or_else(|| {
                            if change.requires_authorization {
                                "Elevation"
                            } else {
                                ""
                            }
                            .into()
                        }))
                })),
        );
    }
    content
}

fn maintenance_summary(
    version: &str,
    scope: SelectedScope,
    install_directory: Option<&str>,
    component_count: usize,
    health: &InstallationHealth,
) -> impl IntoElement {
    let mut summary = div()
        .v_flex()
        .gap_2()
        .child(text_line("Installed version", version, 13.0))
        .child(text_line(
            "Installed for",
            if scope == SelectedScope::User {
                "Current user"
            } else {
                "Everyone"
            },
            13.0,
        ));
    if let Some(directory) = install_directory {
        summary = summary.child(text_line("Location", directory, 13.0));
    }
    summary = summary.child(text_line("Components", &component_count.to_string(), 13.0));
    summary.child(
        div()
            .h_flex()
            .justify_between()
            .text_size(px(13.0))
            .child("Health")
            .child(health.summary.clone()),
    )
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
                        Checkbox::new(format!("modify-component-{}", component.id))
                            .label(component.name.clone())
                            .accessibility_label(if component.required {
                                format!("{} (required)", component.name)
                            } else {
                                component.name.clone()
                            })
                            .checked(selected.contains(&component.id))
                            .disabled(component.required)
                            .on_change(move |checked, _, cx| {
                                row.update(cx, |this, cx| {
                                    if *checked {
                                        this.selected_components.insert(id.clone());
                                    } else {
                                        this.selected_components.remove(&id);
                                    }
                                    this.model.set_component_selected(&id, *checked);
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
        group = group.child(Button::new("update").label("Check for updates").on_click(
            move |_, _, _| {
                let _ = commands.send(UiCommand::Update);
            },
        ));
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

fn status_view(
    model: &UiModel,
    commands: &Sender<UiCommand>,
    details_open: bool,
    theme: &Theme,
    entity: Entity<InstallerView>,
) -> impl IntoElement {
    match model.state {
        ViewState::Running | ViewState::WaitingForSafeCancel => {
            let progress = model.progress.clone().unwrap_or(ProgressModel {
                completed: 0,
                total: 0,
                action: "Preparing…".into(),
                phase: OperationPhase::Prepare,
            });
            let percent = (progress.total > 0).then(|| {
                (progress.completed.min(progress.total) as f32 / progress.total as f32) * 100.0
            });
            let cancel = commands.clone();
            let phase = progress.phase;
            let timeline = div().h_flex().gap_2().children(
                [
                    OperationPhase::Prepare,
                    OperationPhase::Files,
                    OperationPhase::System,
                    OperationPhase::Finish,
                ]
                .into_iter()
                .map(|item| {
                    let active = item == phase;
                    div()
                        .text_size(px(11.0))
                        .text_color(if active {
                            theme.colors.accent
                        } else {
                            theme.colors.muted_foreground
                        })
                        .child(if active {
                            format!("● {}", item.title())
                        } else {
                            item.title().into()
                        })
                }),
            );
            div()
                .v_flex()
                .gap_3()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                    "{} · {}",
                    phase.title(),
                    progress.action
                )))
                .child(
                    Progress::new("operation-progress")
                        .loading(progress.total == 0)
                        .value(percent.unwrap_or(0.0))
                        .accessibility_label(format!(
                            "{}: {}",
                            phase.title(),
                            percent.map_or_else(
                                || "working".into(),
                                |value| format!("{value:.0} percent")
                            )
                        )),
                )
                .child(percent.map_or_else(|| "Working…".into(), |value| format!("{value:.0}%")))
                .child(timeline)
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
                            let _ = cancel.send(UiCommand::Cancel);
                        }),
                )
        }
        ViewState::Blocked => {
            let retry = commands.clone();
            let cancel = commands.clone();
            div()
                .v_flex()
                .gap_3()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Close these applications to continue"),
                )
                .children(
                    model
                        .blockers
                        .iter()
                        .cloned()
                        .map(|item| div().text_size(px(13.0)).child(item)),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(Button::new("retry").primary().label("Retry").on_click(
                            move |_, _, _| {
                                let _ = retry.send(UiCommand::Retry);
                            },
                        ))
                        .child(Button::new("cancel-blocked").label("Cancel").on_click(
                            move |_, _, _| {
                                let _ = cancel.send(UiCommand::Cancel);
                            },
                        )),
                )
        }
        ViewState::ConfirmUninstall => {
            let confirm = commands.clone();
            let dismiss = commands.clone();
            div()
                .v_flex()
                .gap_3()
                .child("Remove this application and its managed resources?")
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(
                            Button::new("confirm-uninstall")
                                .danger()
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
                        ),
                )
        }
        ViewState::Success => {
            let close = commands.clone();
            div()
                .v_flex()
                .gap_3()
                .child(
                    div()
                        .text_color(theme.colors.success)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Finished successfully"),
                )
                .child(
                    Button::new("close")
                        .label("Close")
                        .on_click(move |_, _, _| {
                            let _ = close.send(UiCommand::Close);
                        }),
                )
        }
        ViewState::RecoveryRequired | ViewState::Error => {
            let diagnostic = model.diagnostic.clone().unwrap_or_else(|| {
                DiagnosticPresentation::from_message(
                    model.error.as_deref().unwrap_or("The operation failed"),
                    matches!(model.state, ViewState::RecoveryRequired),
                )
            });
            let retry = commands.clone();
            let close = commands.clone();
            let log = commands.clone();
            let copy = commands.clone();
            let details_entity = entity.clone();
            let mut content = div()
                .v_flex()
                .gap_3()
                .text_color(theme.colors.danger)
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(diagnostic.title.clone()),
                )
                .child(diagnostic.meaning.clone())
                .child(diagnostic.recovery.clone());
            if details_open && let Some(details) = &diagnostic.technical_details {
                content = content.child(
                    div()
                        .p_3()
                        .rounded(px(8.0))
                        .bg(theme.colors.muted)
                        .text_size(px(12.0))
                        .child(details.clone()),
                );
            }
            content
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(
                            Button::new("error-retry")
                                .primary()
                                .label("Retry")
                                .on_click(move |_, _, _| {
                                    let _ = retry.send(UiCommand::Retry);
                                }),
                        )
                        .child(Button::new("error-close").label("Close").on_click(
                            move |_, _, _| {
                                let _ = close.send(UiCommand::Close);
                            },
                        )),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(
                            Button::new("show-details")
                                .label(if details_open {
                                    "Hide details"
                                } else {
                                    "Show details"
                                })
                                .on_click(move |_, _, cx| {
                                    details_entity.update(cx, |view, cx| {
                                        view.details_open = !view.details_open;
                                        cx.notify();
                                    })
                                }),
                        )
                        .child(
                            Button::new("copy-diagnostics")
                                .label("Copy diagnostics")
                                .on_click(move |_, _, _| {
                                    let _ = copy.send(UiCommand::CopyDiagnostics);
                                }),
                        )
                        .child(Button::new("open-log").label("Open log").on_click(
                            move |_, _, _| {
                                let _ = log.send(UiCommand::OpenLog);
                            },
                        )),
                )
        }
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

fn repair_summary(model: &UiModel, theme: &Theme) -> impl IntoElement {
    if model.repair_drift.is_empty() {
        div()
    } else {
        div()
            .v_flex()
            .gap_1()
            .text_color(theme.colors.warning)
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
                install_directory: None,
                allow_directory_override: false,
                estimated_bytes: 0,
                requires_authorization: false,
                preview: None,
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
    fn simple_installer_has_no_customization_surface() {
        let model = installer();
        assert!(!model.has_customization());
    }

    #[test]
    fn author_gated_directory_is_a_lifecycle_selection() {
        let mut model = installer();
        let Surface::Installer { install, .. } = &mut model.surface else {
            panic!()
        };
        install.allow_directory_override = true;
        assert!(model.set_install_directory(Some(r"C:\Apps\Acme".into())));
        assert!(model.has_customization());
    }

    #[test]
    fn plan_preview_and_diagnostics_are_first_class_state() {
        let mut model = installer();
        let preview = PlanPreview {
            application: "Acme".into(),
            version: "1.0.0".into(),
            scope: SelectedScope::User,
            install_directory: r"C:\Apps\Acme".into(),
            selected_components: vec![],
            estimated_bytes: 42,
            download_bytes: 0,
            requires_authorization: false,
            groups: vec![],
            requirements: vec![],
        };
        model.apply(UiEvent::PlanReady(preview));
        let Surface::Installer { install, .. } = &model.surface else {
            panic!()
        };
        assert_eq!(install.preview.as_ref().unwrap().estimated_bytes, 42);
        model.apply(UiEvent::Error {
            message: "blocked by running applications".into(),
            recovery_required: false,
        });
        assert_eq!(
            model.diagnostic.unwrap().kind,
            zup_presentation::DiagnosticKind::Blocked
        );
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
                install_directory: None,
                allow_directory_override: false,
                estimated_bytes: 0,
                requires_authorization: false,
                preview: None,
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
        model.apply(UiEvent::Runtime(RuntimeEvent::ResourceBlocked {
            detail: "Editor.exe\nAgent.exe".into(),
            pids: vec![4820, 7312],
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
    fn reboot_and_recovery_terminal_events_are_not_reported_as_success() {
        let mut model = installer();
        model.apply(UiEvent::Runtime(RuntimeEvent::Completed {
            outcome: "reboot_required".into(),
        }));
        assert_eq!(model.state, ViewState::Error);
        let mut model = installer();
        model.apply(UiEvent::Runtime(RuntimeEvent::Completed {
            outcome: "recovery_required".into(),
        }));
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
    fn progress_keeps_safe_cancellation_state_visible() {
        let mut model = installer();
        model.begin_operation();
        model.request_cancel();
        model.apply(UiEvent::Progress {
            completed: 1,
            total: 2,
            action: "Installing files".into(),
        });
        assert_eq!(model.state, ViewState::WaitingForSafeCancel);
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
    use std::process::Command;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use zup_core::{Privilege, RelativePath, ResourceKey, TargetTriple, hash_reader};
    use zup_platform::TargetPath;
    use zup_runtime::{CancellationHandle, RuntimeRequest, run_install_control};
    use zup_transaction::{
        FileDelta, FilePrecondition, FileWork, TransactionInput, compile_transaction,
    };

    fn smoke_request() -> (RuntimeRequest, zup_windows::WindowsRuntimeBackend) {
        let root = TempDir::new().unwrap().keep();
        let payload = root.join("payload");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("zup-smoke.bin"), b"smoke payload").unwrap();
        let destination = root.join("installed").join("zup-smoke.bin");
        let digest = hash_reader(&b"smoke payload"[..]).unwrap().1;
        let source_relative = RelativePath::new("zup-smoke.bin").unwrap();
        let destination_text = destination.to_string_lossy().into_owned();
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let mut input = TransactionInput::new(target.clone());
        input.files.push(FileWork {
            key: ResourceKey::File {
                destination: destination_text.clone(),
            },
            source_relative,
            destination: TargetPath::new(target.clone(), &destination_text).unwrap(),
            precondition: FilePrecondition::Absent,
            expected_sha256: digest,
            expected_size: 13,
            privilege: Privilege::User,
            delta: FileDelta::Create,
        });
        let request = RuntimeRequest {
            target,
            app_id: zup_core::AppId::new("com.zup.ui-smoke").unwrap(),
            app_version: "1.0.0".parse().unwrap(),
            scope: SelectedScope::User,
            transaction_plan: compile_transaction(&input).unwrap(),
            state_root: root.join("state"),
            work_root: root.join("work"),
            recovery_id: None,
            bootstrap: None,
        };
        let backend = zup_windows::WindowsRuntimeBackend::from_path(payload, None).unwrap();
        (request, backend)
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
                    let (request, backend) = smoke_request();
                    runtime.block_on(run_install_control(
                        &backend,
                        request,
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
                    install_directory: None,
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
                    install_directory: None,
                    allow_directory_override: false,
                    estimated_bytes: 0,
                    requires_authorization: false,
                    preview: None,
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
                    install_directory: None,
                    allow_directory_override: false,
                    estimated_bytes: 0,
                    requires_authorization: false,
                    preview: None,
                },
            }),
            commands,
            events: std::sync::Arc::new(std::sync::Mutex::new(event_rx)),
            selected_components: BTreeSet::new(),
            selected_scope: SelectedScope::User,
            initialized_components: false,
            editing_components: false,
            selected_install_directory: None,
            install_directory_input: None,
            customizing: false,
            showing_changes: false,
            details_open: false,
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
        let (request, backend) = smoke_request();
        let outcome = runtime
            .block_on(run_install_control(
                &backend,
                request,
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
            scope: SelectedScope::User,
            install_directory: None,
            health: InstallationHealth {
                state: "Ready".into(),
                summary: "Up to date".into(),
                drift_count: 0,
            },
        });
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, RuntimeEvent::Completed { .. }) {
                model.apply(UiEvent::Runtime(event));
            }
        }
        assert_eq!(model.state, ViewState::Success);
    }
}

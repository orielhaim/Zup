//! The controls, and what they do to the simulated machine.
//!
//! These are controls for a host, not a second API for a preset. Nothing here
//! reaches the child, and nothing the child can observe says a control was used:
//! a control causes the same event an engine would have caused, the state machine
//! decides what that means, and the result is published through the ordinary
//! session. That is what keeps a third-party preset honest - it is developed
//! against the same wire messages, and there is no path by which it learns it is
//! being developed against.
//!
//! One set, because there is one thing being controlled. A preset author running
//! `zup preset dev` and an application author running `zup preview` are both driving
//! the same machine, and two lists of controls would drift into two different
//! sets of states a real installation could be in.
//!
//! Which matters most for the layouts below. A component list is one of the
//! shapes a real application actually has, and a preset author has to be able to
//! see all four, because a layout with one component hides the fact that a
//! preset's component list has no heading, no scrolling, and no way to say "and
//! four more".

use zup_runtime::{InstallOutcome, RuntimeEvent};
use zup_preset_protocol::{ComponentOption, InstallScope, InstallationHealth, UpdateState};

use crate::machine::{Scenario, Surface};
use crate::simulator::Simulator;

/// The component shapes a real application can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Components {
    /// An application that offers nothing to choose.
    None,
    /// One optional component beside a required one.
    OneOptional,
    /// Enough components that a list has to cope with a lot of them.
    Many,
    /// A required component and several optional ones.
    RequiredAndOptional,
}

impl Components {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::OneOptional => "one-optional",
            Self::Many => "many",
            Self::RequiredAndOptional => "required-and-optional",
        }
    }

    /// The options this shape puts on the surface.
    pub fn options(self) -> Vec<ComponentOption> {
        let component = |id: &str, name: &str, required: bool, selected: bool| ComponentOption {
            id: zup_preset_protocol::ComponentId::new(id).expect("a component id is never empty"),
            name: name.to_owned(),
            description: None,
            required,
            selected,
            installed: false,
        };
        match self {
            Self::None => Vec::new(),
            Self::OneOptional => vec![
                component("core", "Acme", true, true),
                component("docs", "Documentation", false, false),
            ],
            Self::RequiredAndOptional => vec![
                component("core", "Acme", true, true),
                component("docs", "Documentation", false, true),
                component("examples", "Examples", false, false),
                component("source", "Source code", false, false),
            ],
            Self::Many => (0..12)
                .map(|index| {
                    component(
                        &format!("part{index}"),
                        &format!("Component {index}"),
                        index == 0,
                        index < 3,
                    )
                })
                .collect(),
        }
    }
}

/// One thing a person can do to the simulated machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Open one of the named states in [`crate::catalog`].
    Scenario(String),
    Surface(Surface),
    Scope(InstallScope),
    Components(Components),
    /// Run a lifecycle, as a real operation would.
    Run,
    /// Report the engine events an install passes through, one step at a time.
    Advance,
    /// The running transaction committed.
    Commit,
    /// The running operation failed with this engine message.
    Fail(String),
    Blocked(String),
    Rollback,
    RecoveryRequired,
    RebootRequired,
    Busy,
    Update(UpdateState),
    Drift(Vec<String>),
    /// The preview is finished.
    Quit,
}

/// The commands, as a person reads them.
pub const COMMANDS: &str = "\
scenario <name>             open a named state; `scenario` alone lists them
install | maintenance        which surface this machine shows
user | machine              who the installation is for
components <layout>         none | one-optional | many | required-and-optional
run                         start an installation
next                        step the running installation along
commit                      the running operation finished
fail <text>                 the running operation failed with this message
blocked <text>              a file in the way
rollback                    the transaction failed and was undone
recovery                    the last transaction did not finish
reboot                      the machine must restart
busy                        another process is already running one
checking <text>             the update channel is being asked
up-to-date <version>        the channel has nothing newer
available <version>         the channel has a newer release
update-failed <text>        the channel could not be reached
drift <a,b>                 a repair found these resources changed
quit                        stop the preview";

impl Command {
    /// Parse one line of input.
    pub fn parse(line: &str) -> Result<Self, String> {
        let line = line.trim();
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match word {
            "scenario" => Ok(Self::Scenario(rest.to_owned())),
            "commit" => Ok(Self::Commit),
            "fail" => Ok(Self::Fail(rest.to_owned())),
            "install" => Ok(Self::Surface(Surface::Install)),
            "maintenance" => Ok(Self::Surface(Surface::Maintenance)),
            "user" => Ok(Self::Scope(InstallScope::User)),
            "machine" => Ok(Self::Scope(InstallScope::Machine)),
            "components" => match rest {
                "none" => Ok(Self::Components(Components::None)),
                "one-optional" => Ok(Self::Components(Components::OneOptional)),
                "many" => Ok(Self::Components(Components::Many)),
                "required-and-optional" => Ok(Self::Components(Components::RequiredAndOptional)),
                other => Err(format!("`{other}` is not a component layout")),
            },
            "run" => Ok(Self::Run),
            "next" => Ok(Self::Advance),
            "blocked" => Ok(Self::Blocked(rest.to_owned())),
            "rollback" => Ok(Self::Rollback),
            "recovery" => Ok(Self::RecoveryRequired),
            "reboot" => Ok(Self::RebootRequired),
            "busy" => Ok(Self::Busy),
            "checking" => Ok(Self::Update(UpdateState::Checking {
                detail: rest.to_owned(),
            })),
            "up-to-date" => Ok(Self::Update(UpdateState::UpToDate {
                current: rest.to_owned(),
            })),
            "available" => Ok(Self::Update(UpdateState::Available {
                current: "1.4.0".into(),
                available: rest.to_owned(),
            })),
            "update-failed" => Ok(Self::Update(UpdateState::Failed {
                message: rest.to_owned(),
            })),
            "drift" => Ok(Self::Drift(
                rest.split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )),
            "quit" => Ok(Self::Quit),
            "" => Err(String::new()),
            other => Err(format!("`{other}` is not a command")),
        }
    }
}

/// What a control did, for the person who used it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The state changed and the child was told.
    Published,
    /// The state changed and no child is running to tell.
    Held,
    /// A lifecycle was accepted. The engine runs it.
    Running,
    /// The control asked for something the state machine refused.
    Refused(String),
    /// The preview is finished.
    Quit,
}

/// Apply one control to the simulated machine.
pub fn apply(simulator: &mut Simulator, scenario: &mut Scenario, command: Command) -> Effect {
    match command {
        Command::Quit => Effect::Quit,
        Command::Scenario(name) => match crate::catalog::named(&name) {
            Some(machine) => {
                *scenario = machine.scenario().clone();
                *simulator.machine_mut() = machine;
                publish(simulator)
            }
            None => Effect::Refused(format!(
                "`{name}` is not a named state; try one of: {}",
                crate::catalog::ENTRIES
                    .iter()
                    .map(|entry| entry.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
        Command::Commit => {
            simulator.finish_with(&InstallOutcome::Committed);
            publish(simulator)
        }
        Command::Fail(message) => {
            simulator.finish_with(&InstallOutcome::Failed(message));
            publish(simulator)
        }
        Command::Surface(surface) => {
            if scenario.surface != surface {
                scenario.surface = surface;
                simulator.reopen(scenario);
            }
            publish(simulator)
        }
        Command::Scope(scope) => {
            scenario.scope = scope;
            let decision = simulator.act(zup_preset_protocol::Action::SetScope { scope });
            refused_with(simulator, decision)
        }
        Command::Components(layout) => {
            scenario.components = layout.options();
            simulator.reopen(scenario);
            // The first component cannot be turned off, so a layout with a
            // required one has to be opened with it already selected.
            publish(simulator)
        }
        Command::Run => {
            let decision = simulator.act(zup_preset_protocol::Action::Install);
            match decision {
                zup_preset_host::HostDecision::Run { .. } => Effect::Running,
                other => refused_with(simulator, other),
            }
        }
        Command::Advance => {
            for event in operation_events() {
                simulator.observe(&event);
            }
            publish(simulator)
        }
        Command::Blocked(detail) => {
            simulator.observe(&RuntimeEvent::ResourceBlocked {
                detail,
                pids: Vec::new(),
            });
            publish(simulator)
        }
        Command::Rollback => {
            simulator.observe(&RuntimeEvent::RollingBack);
            simulator.finish_with(&InstallOutcome::RolledBack);
            publish(simulator)
        }
        Command::RecoveryRequired => {
            simulator.finish_with(&InstallOutcome::RecoveryRequired);
            publish(simulator)
        }
        Command::RebootRequired => {
            simulator.observe(&RuntimeEvent::RebootRequired {
                id: "a required component".into(),
                exit_code: 3010,
            });
            publish(simulator)
        }
        Command::Busy => {
            simulator.finish_with(&InstallOutcome::Busy {
                operation: "updated",
            });
            publish(simulator)
        }
        Command::Update(state) => {
            simulator.set_update(state);
            publish(simulator)
        }
        Command::Drift(resources) => {
            scenario.health = InstallationHealth::Drifted {
                resources: resources.clone(),
            };
            simulator.set_drift(resources);
            publish(simulator)
        }
    }
}

fn refused_with(simulator: &mut Simulator, decision: zup_preset_host::HostDecision) -> Effect {
    match decision {
        zup_preset_host::HostDecision::Refused(refusal) => Effect::Refused(refusal.to_string()),
        _ => publish(simulator),
    }
}

fn publish(simulator: &mut Simulator) -> Effect {
    match simulator.publish() {
        Ok(()) => Effect::Published,
        Err(error) => Effect::Refused(error.to_string()),
    }
}

/// The events an install reports, in the order it reports them.
///
/// The same events the runtime raises, in the same order, because a control that
/// skipped one would walk through a state a real install cannot be in.
fn operation_events() -> Vec<RuntimeEvent> {
    vec![
        RuntimeEvent::PreflightStarted,
        RuntimeEvent::StagingStarted {
            id: "application".into(),
        },
        RuntimeEvent::Progress {
            completed: 2_048,
            total: 4_096,
            action: "Writing application files".into(),
        },
        RuntimeEvent::OperationStarted {
            id: "application-files".into(),
        },
    ]
}

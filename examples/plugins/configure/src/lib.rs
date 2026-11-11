//! A plugin that records what an installation selected.
//!
//! Plugins return declarations; Zup installs them. Nothing here runs a command
//! or writes a registry key, and the world this plugin implements imports
//! nothing at all - which is what makes a plugin safe to ship inside somebody
//! else's application.

use zup_sdk::plugin::prelude::*;

struct Configure;

impl Plugin for Configure {
    fn plan(context: Context) -> Result<Plan, Error> {
        let mut components = context.selected_components.clone();
        components.sort();

        let report = format!(
            "application: {}\nversion: {}\nplugin: {}\ninstall directory: {}\n\
             scope: {}\nselected components: {}\n",
            context.app_name,
            context.app_version,
            context.plugin_id,
            context.install_directory,
            context.scope.as_str(),
            if components.is_empty() {
                "none".to_owned()
            } else {
                components.join(", ")
            },
        );

        let mut plan =
            Plan::new().generated_file(GeneratedFile::text("configuration", "${install}/configure.txt", report).replacement(FixedReplacement::Quiescent));
        let resources = components.iter().any(|component| component == "resources");
        if resources {
            plan = plan
                .launcher(Launcher::menu("main", "Worker", "${install}/worker"))
                .path_entry(PathEntry::new("cli", "${install}/bin"))
                .data(DataDirectory::path("settings", "${location.shared_data}/${app.id}"));
        }
        if resources || components.iter().any(|component| component == "service") {
            if context.scope != Scope::Machine {
                return Err(Error::unsupported(
                    "the service component requires machine scope",
                ));
            }
            let linux = context.target.starts_with("linux-");
            let dependencies = if components
                .iter()
                .any(|component| component == "dependencies")
            {
                let id = format!("{}.storage", context.app_id);
                plan = plan.service(
                    Service::new(
                        id.clone(),
                        if linux {
                            "${install}/worker"
                        } else {
                            "${install}/worker.exe"
                        },
                        if linux {
                            ServiceExecution::SystemdDynamicUser
                        } else {
                            ServiceExecution::WindowsJob(WindowsServiceAccount::LocalService)
                        },
                    )
                    .with_display_name("Storage")
                    .with_arguments(if linux { Vec::new() } else {
                        vec!["--job".to_owned(), format!("${{job.{id}}}")]
                    })
                    .with_readiness(if linux {
                        ServiceReadiness::Notify
                    } else {
                        ServiceReadiness::Manager
                    })
                    .with_required_files(["${install}/configure.txt"])
                    .with_lifecycle(ServiceLifecycle {
                        install: InitialServiceState::Running,
                        change: ChangedServiceState::Preserve,
                        timeouts: Some(if linux {
                            ServiceTimeouts::Systemd(SystemdTimeouts {
                                start_timeout_seconds: 17,
                                stop_grace_seconds: 23,
                                abort_timeout_seconds: 5,
                                settle_seconds: 5,
                            })
                        } else {
                            ServiceTimeouts::Windows(WindowsTimeouts {
                                start_observation_seconds: 17,
                                stop_observation_seconds: 23,
                            })
                        }),
                    }),
                );
                vec![id]
            } else {
                Vec::new()
            };
            let failure = if components.iter().any(|component| component == "failure") {
                if linux {
                    ServiceFailurePolicy::Systemd(SystemdFailurePolicy {
                        condition: SystemdRestartCondition::OnFailure,
                        delay_seconds: 2,
                        start_limit_burst: 3,
                        start_limit_interval_seconds: 60,
                    })
                } else {
                    ServiceFailurePolicy::Windows(WindowsFailurePolicy {
                        delay_seconds: 2,
                        restart_attempts: 2,
                        reset_seconds: 60,
                    })
                }
            } else {
                ServiceFailurePolicy::None
            };
            plan = plan.service(
                Service::new(
                    format!("{}.worker", context.app_id),
                    if linux {
                        "${install}/worker"
                    } else {
                        "${install}/worker.exe"
                    },
                    if linux {
                        ServiceExecution::SystemdDynamicUser
                    } else {
                        ServiceExecution::WindowsJob(WindowsServiceAccount::LocalService)
                    },
                )
                .with_display_name("Worker")
                .with_arguments(if linux { Vec::new() } else {
                    vec!["--job".to_owned(), format!("${{job.{}.worker}}", context.app_id)]
                })
                .with_readiness(if linux {
                    ServiceReadiness::Notify
                } else {
                    ServiceReadiness::Manager
                })
                .with_failure_policy(failure)
                .with_required_services(dependencies)
                .with_required_files(["${install}/configure.txt"])
                .with_lifecycle(ServiceLifecycle {
                        install: InitialServiceState::Running,
                    change: ChangedServiceState::Preserve,
                    timeouts: Some(if linux {
                        ServiceTimeouts::Systemd(SystemdTimeouts {
                            start_timeout_seconds: 17,
                            stop_grace_seconds: 23,
                            abort_timeout_seconds: 5,
                            settle_seconds: 5,
                        })
                    } else {
                        ServiceTimeouts::Windows(WindowsTimeouts {
                            start_observation_seconds: 17,
                            stop_observation_seconds: 23,
                        })
                    }),
                }),
            );
        }
        if resources {
            plan = plan
                .protocol(Protocol::new("worker", "${install}/worker").with_arguments(["%1"]))
                .file_association(FileAssociation::new(
                    "Worker.Document",
                    ".worker",
                    "${install}/worker",
                ));
        }
        Ok(plan)
    }
}

zup_sdk::plugin::export!(Configure);

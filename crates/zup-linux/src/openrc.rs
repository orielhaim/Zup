use zup_core::{ServiceExecution, ServiceFailurePolicy, ServiceReadiness};
use zup_plan::{DesiredService, ServiceState, TargetPath};

#[cfg(target_os = "linux")]
pub fn validate_executable(bytes: &[u8], target: &zup_core::Target) -> Result<(), String> {
    let executable = zup_binary::Executable::from_bytes(bytes)
        .map_err(|error| format!("OpenRC requires a native ELF service executable: {error}"))?;
    if executable.format() != zup_binary::BinaryFormat::Elf {
        return Err("OpenRC requires a native ELF service executable".into());
    }
    executable
        .refuse_target(target)
        .map_err(|error| error.to_string())
}

pub fn service_name(id: &zup_core::ServiceId) -> String {
    let encoded: String = id
        .as_str()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("zup-openrc-{encoded}")
}

fn quote(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("OpenRC service values cannot contain control characters".into());
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

pub fn registration(service: &DesiredService<TargetPath>) -> ServiceState {
    ServiceState::Registration {
        native_timeouts: service.lifecycle.timeouts_for(&service.execution).native(),
        execution: service.execution.clone(),
        failure: service.failure,
        dependencies: service.dependency_names.clone(),
        display_name: service
            .display_name
            .as_ref()
            .unwrap_or(&service.name)
            .to_string(),
        command: Box::new(service.command.clone()),
        start: service.start,
        readiness: service.readiness,
    }
}

pub fn render(name: &str, state: &ServiceState) -> Result<Vec<u8>, String> {
    let ServiceState::Registration {
        native_timeouts,
        execution,
        failure,
        dependencies,
        display_name,
        command,
        start,
        readiness,
    } = state
    else {
        return Err("OpenRC requires a service registration".into());
    };
    let ServiceExecution::OpenrcUser { user, group } = execution else {
        return Err("OpenRC requires an explicit OpenRC user identity".into());
    };
    execution
        .validate_for(*command.executable.target())
        .map_err(str::to_owned)?;
    native_timeouts
        .validate_for(execution)
        .map_err(str::to_owned)?;
    let zup_core::NativeServiceTimeouts::Openrc {
        start_timeout_seconds,
        stop_grace_seconds,
        kill_wait_seconds,
    } = native_timeouts
    else {
        return Err("OpenRC requires native retry settings".into());
    };
    failure
        .validate_for(*command.executable.target())
        .map_err(str::to_owned)?;
    if *readiness != ServiceReadiness::Manager {
        return Err("OpenRC supports native process readiness, not systemd notification".into());
    }
    if !valid_name(name)
        || dependencies.len() > 256
        || dependencies.iter().any(|name| !valid_name(name))
    {
        return Err("invalid OpenRC managed service identity".into());
    }
    let executable =
        crate::lowering::to_host_path(&command.executable).map_err(|error| error.to_string())?;
    let executable = executable
        .to_str()
        .ok_or("OpenRC executable path must be UTF-8")?;
    if !executable.starts_with('/') {
        return Err("OpenRC executable must be absolute".into());
    }
    // OpenRC's default launcher evaluates command and command_args once after sourcing the script.
    let executable = quote(&quote(executable)?)?;
    let arguments = command
        .arguments
        .iter()
        .map(|value| quote(value))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let arguments = quote(&arguments)?;
    let user = quote(
        &group
            .as_ref()
            .map_or_else(|| user.clone(), |group| format!("{user}:{group}")),
    )?;
    let metadata =
        zup_core::base64_encode(&serde_json::to_vec(state).map_err(|error| error.to_string())?);
    let supervisor = match failure {
        ServiceFailurePolicy::None => {
            "supervise_daemon_args='--pidfd-control --no-respawn --respawn-delay-step 0'\n"
                .to_owned()
        }
        ServiceFailurePolicy::Openrc {
            delay_seconds,
            max_respawns,
            respawn_period_seconds,
        } => format!(
            "supervise_daemon_args='--pidfd-control --respawn-delay-step 0'\nrespawn_delay={delay_seconds}\nrespawn_max={max_respawns}\nrespawn_period={respawn_period_seconds}\n"
        ),
        _ => return Err("OpenRC requires its native respawn policy".into()),
    };
    let supervisor = format!(
        "{supervisor}supervise_daemon_args=\"$supervise_daemon_args --start-timeout {start_timeout_seconds}\"\n"
    );
    let dependency = if dependencies.is_empty() {
        String::new()
    } else {
        format!(
            "depend() {{ need {}; }}\n",
            dependencies
                .iter()
                .map(|value| quote(value))
                .collect::<Result<Vec<_>, _>>()?
                .join(" ")
        )
    };
    let shutdown_wait = stop_grace_seconds + kill_wait_seconds + 5;
    let shutdown = format!(
        "stop() {{ supervise-daemon \"$RC_SVCNAME\" --shutdown --shutdown-timeout {shutdown_wait}; }}\n"
    );
    let activation = if *start == zup_core::ServiceStart::Disabled {
        "start_pre() { return 1; }\n"
    } else {
        r#"start_pre() {
    local membership
    mountinfo -q -f '^cgroup2$' /sys/fs/cgroup || return 1
    read -r membership < /proc/self/cgroup || return 1
    [ "$membership" = "0::/openrc.$RC_SVCNAME" ]
}
"#
    };
    Ok(format!("#!/sbin/openrc-run\n# zup-registration={metadata}\ndescription={}\ncommand={executable}\ncommand_args={arguments}\ncommand_args_foreground=''\ncommand_user={user}\npidfile='/run/{name}.pid'\nretry='TERM/{stop_grace_seconds}/KILL/{kill_wait_seconds}'\nstopgroup=yes\nrc_cgroup_mode=unified\nrc_cgroup_cleanup=NO\numask=027\nno_new_privs=yes\nsupervisor=supervise-daemon\n{supervisor}{dependency}{activation}{shutdown}", quote(display_name)?).into_bytes())
}

#[cfg(any(target_os = "linux", test))]
pub fn parse(name: &str, bytes: &[u8]) -> Result<ServiceState, String> {
    if bytes.len() > 64 * 1024 {
        return Err("OpenRC definition exceeds 64 KiB".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let metadata = text
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix("# zup-registration="))
        .ok_or("OpenRC definition lacks typed registration")?;
    let bytes_json = zup_core::base64_decode(metadata).map_err(|error| error.to_string())?;
    let state: ServiceState =
        serde_json::from_slice(&bytes_json).map_err(|error| error.to_string())?;
    if render(name, &state)? != bytes {
        return Err("OpenRC definition differs from its canonical registration".into());
    }
    Ok(state)
}

pub fn valid_name(name: &str) -> bool {
    name.strip_prefix("zup-openrc-").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.len() <= 192
            && suffix.len().is_multiple_of(2)
            && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

#[cfg(target_os = "linux")]
pub fn resource_name(key: &zup_core::ResourceKey) -> Result<String, String> {
    match key {
        zup_core::ResourceKey::Service { id } => Ok(service_name(id)),
        _ => Err("managed service resource required".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ServiceState {
        ServiceState::Registration {
            native_timeouts: zup_core::NativeServiceTimeouts::Openrc {
                start_timeout_seconds: 90,
                stop_grace_seconds: 90,
                kill_wait_seconds: 5,
            },
            execution: ServiceExecution::OpenrcUser {
                user: "nobody".into(),
                group: Some("nobody".into()),
            },
            failure: ServiceFailurePolicy::None,
            dependencies: vec![service_name(&zup_core::ServiceId::new("database").unwrap())],
            display_name: "Application".into(),
            command: Box::new(zup_plan::CommandSpec::new(
                TargetPath::new(
                    zup_core::Target::parse("linux-x64-musl").unwrap(),
                    "/opt/application/service",
                )
                .unwrap(),
                vec!["literal '$() `id` ; argument".into()],
            )),
            start: zup_core::ServiceStart::Automatic,
            readiness: ServiceReadiness::Manager,
        }
    }

    #[test]
    fn only_the_canonical_definition_authorizes_native_control() {
        let name = service_name(&zup_core::ServiceId::new("application").unwrap());
        let state = state();
        let mut bytes = render(&name, &state).unwrap();
        assert_eq!(parse(&name, &bytes).unwrap(), state);
        bytes.extend_from_slice(b"\nstart() { arbitrary_command; }\n");
        assert!(parse(&name, &bytes).is_err());
    }

    #[rstest::rstest]
    #[case("line\nbreak")]
    #[case("nul\0byte")]
    #[case("carriage\rreturn")]
    fn control_characters_cannot_escape_script_fields(#[case] argument: &str) {
        let mut state = state();
        let ServiceState::Registration { command, .. } = &mut state else {
            unreachable!()
        };
        command.arguments = vec![argument.into()];
        assert!(
            render(
                &service_name(&zup_core::ServiceId::new("application").unwrap()),
                &state
            )
            .is_err()
        );
    }
}

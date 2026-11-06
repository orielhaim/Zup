use zup_core::{ServiceExecution, ServiceFailurePolicy, ServiceReadiness};
use zup_plan::{DesiredService, ServiceState, TargetPath};

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
        execution: service.execution.clone(),
        failure: service.failure,
        dependencies: service.dependency_names.clone(),
        display_name: service
            .display_name
            .as_ref()
            .unwrap_or(&service.name)
            .to_string(),
        command: service.command.clone(),
        start: service.start,
        readiness: service.readiness,
    }
}

pub fn render(name: &str, state: &ServiceState) -> Result<Vec<u8>, String> {
    let ServiceState::Registration {
        execution,
        failure,
        dependencies,
        display_name,
        command,
        start: _,
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
    let generation_hook = r#"start_post() {
    local pid stat
    read -r pid < "$pidfile" || return 1
    case "$pid" in ''|*[!0-9]*) return 1 ;; esac
    stat=$(cat "/proc/$pid/stat") || return 1
    stat=${stat##*) }
    set -- $stat
    [ "$#" -ge 20 ] || return 1
    shift 19
    service_set_value zup_resource "$RC_SVCNAME" || return 1
    service_set_value zup_pid "$pid" || return 1
    service_set_value zup_start_ticks "$1"
}
"#;
    let supervisor = match failure {
        ServiceFailurePolicy::None => "supervisor=''\ncommand_background=yes\n".to_owned(),
        ServiceFailurePolicy::Openrc {
            delay_seconds,
            max_respawns,
            respawn_period_seconds,
        } => format!(
            "supervisor=supervise-daemon\nrespawn_delay={delay_seconds}\nrespawn_max={max_respawns}\nrespawn_period={respawn_period_seconds}\n"
        ),
        _ => return Err("OpenRC requires its native respawn policy".into()),
    };
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
    Ok(format!("#!/sbin/openrc-run\n# zup-registration={metadata}\ndescription={}\ncommand={executable}\ncommand_args={arguments}\ncommand_args_foreground=''\ncommand_user={user}\npidfile='/run/{name}.pid'\nretry='TERM/30/KILL/5'\nstopgroup=yes\numask=027\nno_new_privs=yes\n{supervisor}{dependency}{generation_hook}", quote(display_name)?).into_bytes())
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
            execution: ServiceExecution::OpenrcUser {
                user: "nobody".into(),
                group: Some("nobody".into()),
            },
            failure: ServiceFailurePolicy::None,
            dependencies: vec![service_name(&zup_core::ServiceId::new("database").unwrap())],
            display_name: "Application".into(),
            command: zup_plan::CommandSpec::new(
                TargetPath::new(
                    zup_core::Target::parse("linux-x64-musl").unwrap(),
                    "/opt/application/service",
                )
                .unwrap(),
                vec!["literal '$() `id` ; argument".into()],
            ),
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

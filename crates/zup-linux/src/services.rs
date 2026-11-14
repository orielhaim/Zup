use crate::error::PlanError;
use zup_core::CommandSpec;
use zup_core::{ServiceId, ServiceStart};
use zup_plan::DesiredService;

pub const MINIMUM_SYSTEMD_VERSION: u32 = 254;

pub fn parse_manager_version(raw: &str) -> Option<u32> {
    let digits: String = raw
        .bytes()
        .take_while(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

pub const SERVICE_TYPE: &str = "exec";

pub const WANTED_BY: &str = "multi-user.target";

pub const SYSTEMD_UNIT_DIR: &str = "/usr/local/lib/systemd/system";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnit {
    pub unit: String,

    pub source_path: String,

    // Rendered for the Linux runtime; other hosts validate shapes, not bytes.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub bytes: Vec<u8>,

    pub sha256: zup_core::Sha256Digest,

    pub start: ServiceStart,

    pub id: ServiceId,

    pub display_name: String,
}

impl SystemdUnit {
    pub fn derive(service: &DesiredService<zup_core::TargetPath>) -> Result<Self, PlanError> {
        let unit = service.name.to_string();
        let display_name = service
            .display_name
            .as_ref()
            .map(|name| name.to_string())
            .unwrap_or_else(|| service.name.to_string());
        let bytes = render_unit(service, &display_name)?;
        let (size, sha256) =
            zup_core::hash_reader(bytes.as_slice()).map_err(|error| PlanError::ServiceRefused {
                id: service.id.to_string(),
                reason: format!("rendered unit does not hash: {error}"),
            })?;
        debug_assert_eq!(size, bytes.len() as u64);
        Ok(Self {
            source_path: format!("{SYSTEMD_UNIT_DIR}/{unit}"),
            unit,
            bytes,
            sha256,
            start: service.start,
            id: service.id.clone(),
            display_name,
        })
    }
}

pub(crate) fn dynamic_user_name(unit: &str) -> String {
    let digest = zup_core::hash_bytes(unit.as_bytes());
    format!(
        "zu_{}",
        digest.as_bytes()[..14]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

pub fn render_unit(
    service: &DesiredService<zup_core::TargetPath>,
    display_name: &str,
) -> Result<Vec<u8>, PlanError> {
    let executable =
        host_executable(&service.command).map_err(|reason| PlanError::ServiceRefused {
            id: service.id.to_string(),
            reason,
        })?;
    let definition = UnitDefinition {
        state_directories: service.state_directories.clone(),
        native_timeouts: service.lifecycle.timeouts_for(&service.execution).native(),
        display_name: display_name.into(),
        argv: std::iter::once(executable)
            .chain(service.command.arguments.iter().cloned())
            .collect(),
        dependencies: service.dependency_names.clone(),
        readiness: service.readiness,
        failure: service.failure,
        execution: service.execution.clone(),
    };
    definition
        .render(*service.command.executable.target(), service.name.as_str())
        .map_err(|reason| PlanError::ServiceRefused {
            id: service.id.to_string(),
            reason,
        })
}

pub struct UnitDefinition {
    pub state_directories: Vec<zup_core::RelativePath>,
    pub native_timeouts: zup_core::NativeServiceTimeouts,
    pub failure: zup_core::ServiceFailurePolicy,
    pub dependencies: Vec<String>,
    pub display_name: String,
    pub argv: Vec<String>,
    pub readiness: zup_core::ServiceReadiness,
    pub execution: zup_core::ServiceExecution,
}

#[cfg(target_os = "linux")]
pub(crate) fn render_registration(
    unit: &str,
    state: &zup_plan::ServiceState,
) -> Result<Vec<u8>, String> {
    let zup_plan::ServiceState::Registration {
        state_directories,
        native_timeouts,
        failure,
        dependencies,
        display_name,
        command,
        readiness,
        execution,
        ..
    } = state
    else {
        return Err("service registration required".into());
    };
    UnitDefinition {
        state_directories: state_directories.clone(),
        native_timeouts: *native_timeouts,
        failure: *failure,
        dependencies: dependencies.clone(),
        display_name: display_name.clone(),
        argv: std::iter::once(host_executable(command)?)
            .chain(command.arguments.iter().cloned())
            .collect(),
        readiness: *readiness,
        execution: execution.clone(),
    }
    .render(*command.executable.target(), unit)
}

impl UnitDefinition {
    fn render(&self, target: zup_core::Target, unit: &str) -> Result<Vec<u8>, String> {
        if self.dependencies.len() > 256
            || self.dependencies.iter().any(|name| {
                !name.ends_with(".service")
                    || name.bytes().any(|byte| {
                        !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.' | b'\\')
                    })
            })
        {
            return Err("invalid managed unit dependency".into());
        }
        let dependencies = if self.dependencies.is_empty() {
            String::new()
        } else {
            format!(
                "Requires={}\nAfter={}\n",
                self.dependencies.join(" "),
                self.dependencies.join(" ")
            )
        };
        self.failure.validate_for(target).map_err(str::to_owned)?;
        let (restart, delay, interval, burst) = match self.failure {
            zup_core::ServiceFailurePolicy::None => ("no", 0, 0, 0),
            zup_core::ServiceFailurePolicy::Systemd {
                condition,
                delay_seconds,
                start_limit_burst,
                start_limit_interval_seconds,
            } => (
                match condition {
                    zup_core::SystemdRestartCondition::OnFailure => "on-failure",
                    zup_core::SystemdRestartCondition::OnAbnormal => "on-abnormal",
                },
                delay_seconds,
                start_limit_interval_seconds,
                start_limit_burst,
            ),
            _ => return Err("systemd requires a systemd runtime failure policy".into()),
        };
        let description = render_description(&self.display_name)?;
        let (executable, arguments) = self
            .argv
            .split_first()
            .ok_or("service executable is missing")?;
        let exec_start = render_exec_start(executable, arguments)?;
        self.execution.validate_for(target).map_err(str::to_owned)?;
        let execution = match &self.execution {
            zup_core::ServiceExecution::SystemdDynamicUser => {
                let user = dynamic_user_name(unit);
                format!("DynamicUser=yes\nUser={user}\nGroup={user}\n")
            }
            zup_core::ServiceExecution::SystemdUser { user, group } => {
                let mut value = format!("User={user}\n");
                if let Some(group) = group {
                    value.push_str(&format!("Group={group}\n"));
                }
                value
            }
            zup_core::ServiceExecution::WindowsJob { .. }
            | zup_core::ServiceExecution::OpenrcUser { .. } => {
                return Err("Windows execution identity cannot run under systemd".into());
            }
        };
        let mut state = String::new();
        let mut names = std::collections::BTreeSet::new();
        for name in &self.state_directories {
            if !name.as_str().starts_with("zup-")
                || name.as_str().len() != 68
                || !name.as_str()[4..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || !names.insert(name)
            {
                return Err("invalid or repeated native state namespace".into());
            }
            state.push_str(&format!("StateDirectory={name}\n"));
        }
        if !state.is_empty() {
            state.push_str("StateDirectoryMode=0700\n");
        }
        let service_type = match self.readiness {
            zup_core::ServiceReadiness::Manager => SERVICE_TYPE,
            zup_core::ServiceReadiness::Notify => "notify",
        };
        let notify = if self.readiness == zup_core::ServiceReadiness::Notify {
            "NotifyAccess=main\n"
        } else {
            ""
        };
        self.native_timeouts
            .validate_for(&self.execution)
            .map_err(str::to_owned)?;
        let zup_core::NativeServiceTimeouts::Systemd {
            start_timeout_seconds,
            stop_grace_seconds,
            abort_timeout_seconds,
        } = self.native_timeouts
        else {
            return Err("systemd requires native systemd timeout settings".into());
        };
        Ok(format!("[Unit]\nDescription={description}\n{dependencies}StartLimitIntervalSec={interval}\nStartLimitBurst={burst}\n\n[Service]\nType={service_type}\n{notify}{execution}{state}NoNewPrivileges=yes\nKillMode=control-group\nSendSIGKILL=yes\nRestart={restart}\nRestartSec={delay}\nExecStart={exec_start}\nTimeoutStartSec={start_timeout_seconds}\nTimeoutStopSec={stop_grace_seconds}\nTimeoutAbortSec={abort_timeout_seconds}\n\n[Install]\nWantedBy={WANTED_BY}\n").into_bytes())
    }
}

fn render_description(display: &str) -> Result<String, String> {
    if display.is_empty() {
        return Err("a service description is never empty".into());
    }
    if display
        .bytes()
        .any(|b| b == b'\n' || b == b'\r' || b == b'\0' || b < 0x20)
    {
        return Err("a service description holds no control characters".into());
    }
    Ok(display.replace('%', "%%"))
}

fn host_executable(command: &CommandSpec) -> Result<String, String> {
    let text = crate::lowering::to_host_path(&command.executable)
        .map(|host| host.to_string_lossy().into_owned())
        .map_err(|error| format!("service binary is not a valid Linux path: {error}"))?;
    if !text.starts_with('/') {
        return Err("a service binary is an absolute path".into());
    }
    if text.contains(['\n', '\r', '\0']) || text.bytes().any(|b| b < 0x20) {
        return Err("a service binary holds no control characters".into());
    }
    Ok(text)
}

pub fn render_exec_start(executable: &str, arguments: &[String]) -> Result<String, String> {
    let mut words = Vec::with_capacity(arguments.len() + 1);
    words.push(quote_word(executable)?);
    for argument in arguments {
        words.push(quote_word(argument)?);
    }
    Ok(format!(":{}", words.join(" ")))
}

fn quote_word(word: &str) -> Result<String, String> {
    if word
        .bytes()
        .any(|b| b == b'\n' || b == b'\r' || b == b'\0' || b < 0x20)
    {
        return Err("a service argument holds no control characters".into());
    }
    let mut escaped = String::with_capacity(word.len() + 2);
    for ch in word.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            _ => escaped.push(ch),
        }
    }
    Ok(format!("\"{escaped}\""))
}

pub fn parse_exec_start(line: &str) -> Result<Vec<String>, String> {
    let line = line
        .strip_prefix(':')
        .ok_or("managed ExecStart requires literal argument expansion")?;
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            match ch {
                '"' => {
                    in_quotes = false;
                }
                '\\' => {
                    let next = chars.next().ok_or("dangling escape in ExecStart")?;
                    match next {
                        '\\' => current.push('\\'),
                        '"' => current.push('"'),
                        'n' => current.push('\n'),
                        't' => current.push('\t'),
                        _ => return Err(format!("unsupported escape `\\{next}` in ExecStart")),
                    }
                }
                '%' => {
                    if chars.peek() == Some(&'%') {
                        chars.next();
                        current.push('%');
                    } else {
                        return Err("unescaped `%` in ExecStart".into());
                    }
                }
                _ => current.push(ch),
            }
        } else if ch == '"' {
            in_quotes = true;
            in_word = true;
        } else if ch == ' ' || ch == '\t' {
            if in_word {
                words.push(std::mem::take(&mut current));
                in_word = false;
            }
        } else if ch == '%' {
            if chars.peek() == Some(&'%') {
                chars.next();
                current.push('%');
                in_word = true;
            } else {
                return Err("unescaped `%` in ExecStart".into());
            }
        } else if ch == '\\' {
            let next = chars.next().ok_or("dangling escape in ExecStart")?;
            match next {
                '\\' => current.push('\\'),
                '"' => current.push('"'),
                _ => return Err(format!("unsupported escape `\\{next}` in ExecStart")),
            }
            in_word = true;
        } else {
            current.push(ch);
            in_word = true;
        }
    }
    if in_quotes {
        return Err("unterminated quote in ExecStart".into());
    }
    if in_word {
        words.push(current);
    }
    if words.is_empty() {
        return Err("ExecStart names no executable".into());
    }
    Ok(words)
}

// Parsed back by the Linux runtime and by the render round-trip tests.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_unit(
    bytes: &[u8],
    target: zup_core::Target,
    unit: &str,
) -> Result<UnitDefinition, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "a unit source is UTF-8".to_owned())?;
    let mut restart = None;
    let mut restart_delay = None;
    let mut restart_interval = None;
    let mut restart_burst = None;
    let mut start_timeout = None;
    let mut stop_grace = None;
    let mut abort_timeout = None;
    let mut dependencies = Vec::new();
    let mut description: Option<String> = None;
    let mut exec_start: Option<String> = None;
    let mut readiness = None;
    let mut user = None;
    let mut group = None;
    let mut dynamic = false;
    let mut no_new_privileges = false;
    let mut state_directories = Vec::new();
    let mut section = "";
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = match name {
                "Unit" => "Unit",
                "Service" => "Service",
                "Install" => "Install",
                _ => "",
            };
            continue;
        }
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match (section, key.trim()) {
            ("Service", "StateDirectory") => state_directories
                .push(zup_core::RelativePath::new(value).map_err(|error| error.to_string())?),
            ("Service", "TimeoutStartSec") => start_timeout = value.parse::<u32>().ok(),
            ("Service", "TimeoutStopSec") => stop_grace = value.parse::<u32>().ok(),
            ("Service", "TimeoutAbortSec") => abort_timeout = value.parse::<u32>().ok(),
            ("Service", "Restart") => restart = Some(value.to_owned()),
            ("Service", "RestartSec") => restart_delay = value.parse::<u32>().ok(),
            ("Unit", "StartLimitIntervalSec") => restart_interval = value.parse::<u32>().ok(),
            ("Unit", "StartLimitBurst") => restart_burst = value.parse::<u32>().ok(),
            ("Service", "User") if user.is_none() => user = Some(value.trim().to_owned()),
            ("Service", "Group") if group.is_none() => group = Some(value.trim().to_owned()),
            ("Service", "DynamicUser") if value.trim() == "yes" && !dynamic => dynamic = true,
            ("Service", "NoNewPrivileges") if value.trim() == "yes" && !no_new_privileges => {
                no_new_privileges = true
            }
            ("Service", "Type") => {
                if readiness.is_some() {
                    return Err("duplicate service type".into());
                }
                readiness = Some(match value.trim() {
                    "exec" => zup_core::ServiceReadiness::Manager,
                    "notify" => zup_core::ServiceReadiness::Notify,
                    _ => return Err("unsupported service readiness type".into()),
                });
            }
            ("Unit", "Requires") => {
                dependencies = value.split_ascii_whitespace().map(str::to_owned).collect();
            }
            ("Unit", "Description") => {
                description = Some(value.trim().replace("%%", "%"));
            }
            ("Service", "ExecStart") => {
                exec_start = Some(value.trim().to_owned());
            }
            _ => {}
        }
    }
    let description = description.ok_or("a unit source names a Description")?;
    let exec_start = exec_start.ok_or("a unit source names an ExecStart")?;
    if !no_new_privileges {
        return Err("managed services require NoNewPrivileges=yes".into());
    }
    let execution = match (dynamic, user, group) {
        (true, Some(user), Some(group)) if user == dynamic_user_name(unit) && group == user => {
            zup_core::ServiceExecution::SystemdDynamicUser
        }
        (false, Some(user), group) => zup_core::ServiceExecution::SystemdUser { user, group },
        _ => return Err("service execution identity is missing or inconsistent".into()),
    };
    let failure = match (
        restart.as_deref(),
        restart_delay,
        restart_interval,
        restart_burst,
    ) {
        (Some("no"), Some(0), Some(0), Some(0)) => zup_core::ServiceFailurePolicy::None,
        (
            Some(condition @ ("on-failure" | "on-abnormal")),
            Some(delay_seconds),
            Some(start_limit_interval_seconds),
            Some(start_limit_burst),
        ) => zup_core::ServiceFailurePolicy::Systemd {
            condition: if condition == "on-failure" {
                zup_core::SystemdRestartCondition::OnFailure
            } else {
                zup_core::SystemdRestartCondition::OnAbnormal
            },
            delay_seconds,
            start_limit_burst,
            start_limit_interval_seconds,
        },
        _ => return Err("runtime failure policy is missing or unsupported".into()),
    };
    let definition = UnitDefinition {
        state_directories,
        native_timeouts: zup_core::NativeServiceTimeouts::Systemd {
            start_timeout_seconds: start_timeout.ok_or("startup timeout is required")?,
            stop_grace_seconds: stop_grace.ok_or("shutdown grace is required")?,
            abort_timeout_seconds: abort_timeout.ok_or("abort timeout is required")?,
        },
        failure,
        dependencies,
        display_name: description,
        argv: parse_exec_start(&exec_start)?,
        readiness: readiness.ok_or("service type is required")?,
        execution,
    };
    if definition.render(target, unit)? != bytes {
        return Err("service definition differs from the canonical managed configuration".into());
    }
    Ok(definition)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(
        id: &str,
        executable: &str,
        args: Vec<&str>,
        start: ServiceStart,
    ) -> DesiredService<zup_core::TargetPath> {
        let target = zup_core::Target::parse("linux-x64").unwrap();
        DesiredService {
            state_directories: Vec::new(),
            failure: Default::default(),
            execution: zup_core::ServiceExecution::SystemdUser {
                user: "root".into(),
                group: None,
            },
            requires_files: Vec::new(),
            requires_services: Vec::new(),
            dependency_names: Vec::new(),
            lifecycle: Default::default(),
            readiness: Default::default(),
            key: zup_core::ResourceKey::Service {
                id: ServiceId::new(id).unwrap(),
            },
            id: ServiceId::new(id).unwrap(),
            name: zup_core::NonEmptyString::new(format!("{id}.service")).unwrap(),
            display_name: None,
            command: CommandSpec::new(
                zup_core::TargetPath::new(target, executable).unwrap(),
                args.into_iter().map(|a| a.to_owned()).collect(),
            ),
            start,
            privilege: zup_core::Privilege::System,
        }
    }

    #[rstest::rstest]
    #[case::plain("259", Some(259))]
    #[case::suffixed("259.5-0ubuntu3.4", Some(259))]
    #[case::minimum("254", Some(MINIMUM_SYSTEMD_VERSION))]
    #[case::old("239", Some(239))]
    #[case::empty("", None)]
    #[case::unknown("unknown", None)]
    #[case::prefixed("v259", None)]
    fn manager_versions_parse_to_their_major(#[case] raw: &str, #[case] expected: Option<u32>) {
        assert_eq!(parse_manager_version(raw), expected);
    }

    #[test]
    fn display_names_do_not_change_unit_identity() {
        let first = service("tool", "/opt/acme/tool", vec![], ServiceStart::Automatic);
        let mut second = first.clone();
        second.display_name = Some(zup_core::NonEmptyString::new("New Name").unwrap());
        assert_eq!(
            SystemdUnit::derive(&first).unwrap().unit,
            SystemdUnit::derive(&second).unwrap().unit
        );
    }

    #[test]
    fn rendering_is_deterministic_and_minimal() {
        let desired = SystemdUnit::derive(&service(
            "tool",
            "/opt/acme/tool",
            vec!["--serve"],
            ServiceStart::Automatic,
        ))
        .unwrap();
        let again = SystemdUnit::derive(&service(
            "tool",
            "/opt/acme/tool",
            vec!["--serve"],
            ServiceStart::Automatic,
        ))
        .unwrap();
        assert_eq!(desired.bytes, again.bytes);
        let text = String::from_utf8(desired.bytes.clone()).unwrap();
        assert!(text.contains("Type=exec"), "{text}");
        assert!(text.contains("WantedBy=multi-user.target"), "{text}");
        assert!(
            text.contains("Restart=no\nRestartSec=0\n"),
            "automatic crash recovery is disabled unless declared: {text}"
        );
        assert!(
            text.contains("User=root\nNoNewPrivileges=yes\n"),
            "explicit account and privilege policy: {text}"
        );
        assert!(
            !text.contains("Environment"),
            "no environment directives: {text}"
        );
    }

    #[test]
    fn literal_argv_survives_a_render_round_trip() {
        let args = vec![
            "$FOO",
            "${FOO}",
            "%u",
            "%n",
            "a b",
            "say \"hello\"",
            "back\\slash",
            "",
            "-c",
            "a;b|c&d",
            "世界",
        ];
        let desired = SystemdUnit::derive(&service(
            "tool",
            "/opt/acme/tool",
            args.clone(),
            ServiceStart::Manual,
        ))
        .unwrap();
        let text = String::from_utf8(desired.bytes.clone()).unwrap();
        let exec_line = text
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .unwrap()
            .strip_prefix("ExecStart=")
            .unwrap();
        assert!(
            !exec_line.contains("sh -c"),
            "direct execution, never shell"
        );
        let argv = parse_unit(
            &desired.bytes,
            zup_core::Target::parse("linux-x64").unwrap(),
            &desired.unit,
        )
        .unwrap()
        .argv;
        assert_eq!(argv[0], "/opt/acme/tool");
        assert_eq!(
            argv[1..],
            args.iter().map(|a| a.to_string()).collect::<Vec<_>>()[..]
        );
        assert!(
            exec_line.starts_with(':'),
            "environment expansion neutralized: {exec_line}"
        );
        assert!(
            exec_line.contains("%%u"),
            "specifier expansion neutralized: {exec_line}"
        );
    }

    #[test]
    fn control_values_are_refused() {
        assert!(quote_word("a\nb").is_err());
        assert!(quote_word("a\rb").is_err());
        assert!(render_description("a\nb").is_err());
        assert!(render_description("100%").is_ok());
        assert_eq!(render_description("100%").unwrap(), "100%%");
    }

    #[rstest::rstest]
    #[case::identity("User=root", "User=nobody")]
    #[case::security("NoNewPrivileges=yes", "NoNewPrivileges=no")]
    #[case::extra(
        "NoNewPrivileges=yes",
        "NoNewPrivileges=yes\nAmbientCapabilities=CAP_SYS_ADMIN"
    )]
    #[case::duplicate("User=root", "User=root\nUser=nobody")]
    fn modified_security_configuration_is_not_adopted(#[case] old: &str, #[case] new: &str) {
        let desired = service("tool", "/opt/acme/tool", vec![], ServiceStart::Manual);
        let canonical = render_unit(&desired, "Tool").unwrap();
        let changed = String::from_utf8(canonical).unwrap().replace(old, new);
        let parsed = parse_unit(
            changed.as_bytes(),
            *desired.command.executable.target(),
            desired.name.as_str(),
        );
        if let Ok(parsed) = parsed {
            assert_ne!(parsed.execution, desired.execution);
        }
    }
}

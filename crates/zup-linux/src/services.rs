use crate::error::PlanError;
use zup_core::{ServiceId, ServiceStart};
use zup_platform::{CommandSpec, TargetService};

pub const MINIMUM_SYSTEMD_VERSION: u32 = 240;

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

pub const UNIT_PREFIX: &str = "zup-";

const MAX_UNIT_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredService {
    pub unit: String,

    pub source_path: String,

    pub bytes: Vec<u8>,

    pub sha256: zup_core::Sha256Digest,

    pub start: ServiceStart,

    pub id: ServiceId,

    pub display_name: String,
}

impl DesiredService {
    pub fn derive(service: &TargetService) -> Result<Self, PlanError> {
        let unit = unit_name(&service.id)?;
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
            source_path: format!("{}/{unit}", crate::machine::SYSTEMD_UNIT_DIR),
            unit,
            bytes,
            sha256,
            start: service.start,
            id: service.id.clone(),
            display_name,
        })
    }
}

pub fn unit_name(id: &ServiceId) -> Result<String, PlanError> {
    let raw = id.as_str();
    if raw.is_empty() {
        return Err(PlanError::ServiceRefused {
            id: raw.to_owned(),
            reason: "a service id is never empty".into(),
        });
    }
    let mut escaped = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => {
                escaped.push(byte as char);
            }
            _ => {
                escaped.push_str(&format!("\\x{byte:02x}"));
            }
        }
    }
    let digest = zup_core::hash_bytes(raw.as_bytes());
    let suffix: String = digest.as_bytes()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut stem = format!("{UNIT_PREFIX}{escaped}-{suffix}");
    if stem.len() > MAX_UNIT_LEN - ".service".len() {
        let keep = (MAX_UNIT_LEN - ".service".len()).saturating_sub(suffix.len() + 1);
        let truncated: String = escaped.chars().take(keep.min(escaped.len())).collect();
        stem = format!("{UNIT_PREFIX}{truncated}-{suffix}");
    }
    let unit = format!("{stem}.service");
    if unit.len() > MAX_UNIT_LEN || !is_valid_unit_name(&unit) {
        return Err(PlanError::ServiceRefused {
            id: raw.to_owned(),
            reason: "the derived unit name is not a valid systemd unit name".into(),
        });
    }
    Ok(unit)
}

fn is_valid_unit_name(unit: &str) -> bool {
    let Some(stem) = unit.strip_suffix(".service") else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 200
        && stem.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'\\' | b'x' | b'.')
        })
        && !stem.contains('/')
        && !stem.contains('\0')
}

pub fn render_unit(service: &TargetService, display_name: &str) -> Result<Vec<u8>, PlanError> {
    let id = service.id.to_string();
    let refused = |reason: String| PlanError::ServiceRefused {
        id: id.clone(),
        reason,
    };
    let description = render_description(display_name).map_err(refused)?;
    let executable = host_executable(&service.command).map_err(refused)?;
    let exec_start = render_exec_start(&executable, &service.command.arguments).map_err(refused)?;
    let text = format!(
        "[Unit]\nDescription={description}\n\n[Service]\nType={SERVICE_TYPE}\nExecStart={exec_start}\n\n[Install]\nWantedBy={WANTED_BY}\n"
    );
    Ok(text.into_bytes())
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
    Ok(words.join(" "))
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
            '$' => escaped.push_str("$$"),
            '%' => escaped.push_str("%%"),
            _ => escaped.push(ch),
        }
    }
    Ok(format!("\"{escaped}\""))
}

pub fn parse_exec_start(line: &str) -> Result<Vec<String>, String> {
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
                '$' => {
                    if chars.peek() == Some(&'$') {
                        chars.next();
                        current.push('$');
                    } else {
                        return Err("unescaped `$` in ExecStart".into());
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
        } else if ch == '$' {
            if chars.peek() == Some(&'$') {
                chars.next();
                current.push('$');
                in_word = true;
            } else {
                return Err("unescaped `$` in ExecStart".into());
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

pub fn parse_unit(bytes: &[u8]) -> Result<(String, Vec<String>), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "a unit source is UTF-8".to_owned())?;
    let mut description: Option<String> = None;
    let mut exec_start: Option<String> = None;
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
    Ok((description, parse_exec_start(&exec_start)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(id: &str, executable: &str, args: Vec<&str>, start: ServiceStart) -> TargetService {
        let target = zup_core::TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap();
        TargetService {
            key: zup_core::ResourceKey::Service {
                id: ServiceId::new(id).unwrap(),
            },
            id: ServiceId::new(id).unwrap(),
            name: zup_core::NonEmptyString::new("Tool").unwrap(),
            display_name: None,
            command: CommandSpec::new(
                zup_platform::TargetPath::new(target, executable).unwrap(),
                args.into_iter().map(|a| a.to_owned()).collect(),
            ),
            start,
            privilege: zup_core::Privilege::System,
        }
    }

    #[rstest::rstest]
    #[case::plain("259", Some(259))]
    #[case::suffixed("259.5-0ubuntu3.4", Some(259))]
    #[case::minimum("240", Some(MINIMUM_SYSTEMD_VERSION))]
    #[case::old("239", Some(239))]
    #[case::empty("", None)]
    #[case::unknown("unknown", None)]
    #[case::prefixed("v259", None)]
    fn manager_versions_parse_to_their_major(#[case] raw: &str, #[case] expected: Option<u32>) {
        assert_eq!(parse_manager_version(raw), expected);
    }

    #[test]
    fn unit_names_are_stable_and_injective() {
        let first = unit_name(&ServiceId::new("tool").unwrap()).unwrap();
        let second = unit_name(&ServiceId::new("tool").unwrap()).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("zup-") && first.ends_with(".service"));
        let other = unit_name(&ServiceId::new("tool2").unwrap()).unwrap();
        assert_ne!(first, other);
        let tricky_a = unit_name(&ServiceId::new("a/b").unwrap()).unwrap();
        let tricky_b = unit_name(&ServiceId::new("a\\xb").unwrap()).unwrap();
        assert_ne!(tricky_a, tricky_b, "escaping never merges identities");
        let upper = unit_name(&ServiceId::new("Tool").unwrap()).unwrap();
        assert_ne!(first, upper, "case is significant");
    }

    #[test]
    fn display_names_do_not_change_unit_identity() {
        let first = service("tool", "/opt/acme/tool", vec![], ServiceStart::Automatic);
        let mut second = first.clone();
        second.display_name = Some(zup_core::NonEmptyString::new("New Name").unwrap());
        assert_eq!(
            DesiredService::derive(&first).unwrap().unit,
            DesiredService::derive(&second).unwrap().unit
        );
    }

    #[test]
    fn rendering_is_deterministic_and_minimal() {
        let desired = DesiredService::derive(&service(
            "tool",
            "/opt/acme/tool",
            vec!["--serve"],
            ServiceStart::Automatic,
        ))
        .unwrap();
        let again = DesiredService::derive(&service(
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
            !text.contains("Restart="),
            "no speculative directives: {text}"
        );
        assert!(!text.contains("User="), "no account directives: {text}");
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
        let desired = DesiredService::derive(&service(
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
        let (_description, argv) = parse_unit(&desired.bytes).unwrap();
        assert_eq!(argv[0], "/opt/acme/tool");
        assert_eq!(
            argv[1..],
            args.iter().map(|a| a.to_string()).collect::<Vec<_>>()[..]
        );
        assert!(
            !exec_line.contains("$FOO") || exec_line.contains("$$FOO"),
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
}

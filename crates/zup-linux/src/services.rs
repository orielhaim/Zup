//! Linux lowering of the portable service model into systemd units.
//!
//! Pure rendering: a stable service identity becomes a deterministic unit
//! name, and a portable command becomes deterministic unit bytes. No D-Bus,
//! no filesystem, no process state here - the manager integration in
//! [`crate::systemd`] and the transactional application in the service
//! executor own those. Keeping rendering pure is what lets `zup build` on a
//! Windows host compose exactly the bytes a Linux worker later verifies.
//!
//! Semantics preserved from the portable contract (and the Windows backend):
//! `Automatic` is persistent boot enablement, `Manual` is installed but not
//! enabled, `Disabled` is a persistent mask. Installing never starts the
//! service; the unit file registers boot policy only.
//!
//! Chosen `Type=exec`: stronger startup semantics than `simple` (systemd
//! considers the service started only after the executable is successfully
//! launched), without the handshake protocol `notify`/`dbus`/`forking`
//! would require. `Type=exec` needs systemd 240 or newer, present on every
//! supported target environment.

use zup_core::{ServiceId, ServiceStart};
use zup_platform::{CommandSpec, TargetService};

/// Minimum systemd version the rendered units assume.
pub const MINIMUM_SYSTEMD_VERSION: u32 = 240;

/// Parse a `Manager.Version` string into its major version.
///
/// systemd reports strings like `259` or `259.5-0ubuntu3.4`; only the
/// leading integer run is meaningful here. Anything without leading
/// digits is unparsable, and unparsable fails closed at preflight rather
/// than guessing a version the manager never claimed.
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

/// The `[Service]` type the renderer emits, and why it is here rather than
/// inferred at install time.
pub const SERVICE_TYPE: &str = "exec";

/// The install target every Zup service wants. A system service, not a
/// desktop-session one; never varied by environment.
pub const WANTED_BY: &str = "multi-user.target";

/// Unit-name prefix. Keeps Zup units in one namespace and away from
/// distro-owned names; collisions are still refused, never overwritten.
pub const UNIT_PREFIX: &str = "zup-";

/// Maximum full unit file name length (including `.service`).
const MAX_UNIT_LEN: usize = 200;

/// Why a service has no honest systemd lowering.
#[derive(Debug, thiserror::Error)]
pub enum ServiceRenderError {
    #[error("service `{id}` cannot become a systemd unit: {reason}")]
    Refused { id: String, reason: String },
}

/// The Linux-native desired state for one portable service: everything the
/// privileged worker needs, derived deterministically from the target plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredService {
    /// Stable systemd unit file name, e.g. `zup-tool-1a2b3c4d5e6f.service`.
    pub unit: String,
    /// Canonical unit source path text (host spelling).
    pub source_path: String,
    /// Deterministic unit bytes.
    pub bytes: Vec<u8>,
    /// Digest of `bytes`.
    pub sha256: zup_core::Sha256Digest,
    /// Desired persistent start policy.
    pub start: ServiceStart,
    /// Service identity as planned.
    pub id: ServiceId,
    /// Display name as rendered into `Description=`.
    pub display_name: String,
}

impl DesiredService {
    /// Derive the desired state for one target service. Pure and
    /// deterministic: the same target plan always yields the same bytes.
    pub fn derive(service: &TargetService) -> Result<Self, ServiceRenderError> {
        let unit = unit_name(&service.id)?;
        let display_name = service
            .display_name
            .as_ref()
            .map(|name| name.to_string())
            .unwrap_or_else(|| service.name.to_string());
        let bytes = render_unit(service, &display_name)?;
        let (size, sha256) = zup_core::hash_reader(bytes.as_slice()).map_err(|error| {
            ServiceRenderError::Refused {
                id: service.id.to_string(),
                reason: format!("rendered unit does not hash: {error}"),
            }
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

/// Derive the deterministic unit name for a stable service identity.
///
/// The name depends only on the portable [`ServiceId`]: display-name,
/// application, version, and install-path changes never rename the unit.
/// Escaping is injective over the id bytes (unreserved bytes pass through,
/// everything else becomes `\xHH`), plus a 12-hex-char hash suffix so a
/// truncation can never merge two identities. Case is preserved: Linux is
/// case-sensitive and folding would refuse installations the manager accepts.
pub fn unit_name(id: &ServiceId) -> Result<String, ServiceRenderError> {
    let raw = id.as_str();
    if raw.is_empty() {
        return Err(ServiceRenderError::Refused {
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
        return Err(ServiceRenderError::Refused {
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

/// Render the deterministic unit source for one target service.
pub fn render_unit(
    service: &TargetService,
    display_name: &str,
) -> Result<Vec<u8>, ServiceRenderError> {
    let id = service.id.to_string();
    let refused = |reason: String| ServiceRenderError::Refused {
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

/// Escape a user-visible name into a `Description=` value.
///
/// Newlines and controls are refused (no unit-directive injection); `%` is
/// doubled so no specifier expansion is introduced through a display name.
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

/// The executable as a host path: absolute, literal, never a shell word.
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

/// Render `ExecStart=` from a literal argv: direct execution, never shell.
///
/// Every word is double-quoted after escaping (`\\` → `\\\\`, `"` → `\\\"`),
/// with systemd's `$` expansion neutralized (`$` → `$$`) and specifier
/// expansion neutralized (`%` → `%%`). Quoting every word (including the
/// executable) keeps systemd's `-`/`@`/`:`/`+`/`!` command prefixes inside
/// the quotes, where they are literal path bytes rather than control
/// prefixes. Empty arguments render as `""`. Control/newline values are
/// refused rather than guessed.
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

/// Parse our deterministic `ExecStart=` rendering back into argv.
///
/// Accepts the double-quoted subset the renderer emits (plus bare words for
/// tolerance), reversing `\\`, `\"`, `$$` → `$`, `%%` → `%`. Anything outside
/// the subset is refused: an administrator-edited unit is drift, not a second
/// rendering dialect to support.
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

/// Parse a rendered unit's `Description=`/`ExecStart=` back into semantics.
///
/// Used by the snapshot to compare observed state against desired state with
/// the portable planner. Returns `(display_name, argv)`.
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

    #[test]
    fn manager_versions_parse_to_their_major() {
        assert_eq!(parse_manager_version("259"), Some(259));
        assert_eq!(parse_manager_version("259.5-0ubuntu3.4"), Some(259));
        assert_eq!(parse_manager_version("240"), Some(MINIMUM_SYSTEMD_VERSION));
        assert_eq!(parse_manager_version("239"), Some(239));
        assert_eq!(parse_manager_version(""), None);
        assert_eq!(parse_manager_version("unknown"), None);
        assert_eq!(parse_manager_version("v259"), None);
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

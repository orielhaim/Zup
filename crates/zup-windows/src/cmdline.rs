use std::path::Path;

use zup_core::TargetTriple;
use zup_platform::{CommandSpec, TargetPath};

pub fn format_command_line(executable: &Path, arguments: &[String]) -> String {
    let mut line = String::new();
    line.push_str(&quote_arg(&executable.to_string_lossy()));
    for arg in arguments {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_owned();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

pub fn parse_command_line(command_line: &str) -> (Option<String>, Vec<String>) {
    let mut args = split_command_line(command_line);
    if args.is_empty() {
        return (None, Vec::new());
    }
    let exe = args.remove(0);
    (Some(exe), args)
}

pub fn split_command_line(command_line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = command_line.chars().peekable();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                started = true;
                in_quotes = !in_quotes;
            }
            '\\' => {
                let mut backslashes = 1usize;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    backslashes += 1;
                }
                if chars.peek() == Some(&'"') {
                    chars.next();
                    started = true;
                    current.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        current.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                } else {
                    started = true;
                    current.extend(std::iter::repeat_n('\\', backslashes));
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if started || !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                started = true;
                current.push(c);
            }
        }
    }
    if started || !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub fn commands_match(a: &CommandSpec, b: &CommandSpec) -> bool {
    a.executable.equivalent(&b.executable) && a.arguments == b.arguments
}

pub fn command_spec_from_command_line(
    command_line: &str,
    target: &TargetTriple,
) -> Result<CommandSpec, String> {
    let (exe, arguments) = parse_command_line(command_line);
    let exe = exe.ok_or_else(|| "empty command line".to_owned())?;
    if exe.is_empty() {
        return Err("empty executable".to_owned());
    }
    let executable = TargetPath::new(target.clone(), exe).map_err(|error| error.to_string())?;
    Ok(CommandSpec {
        executable,
        arguments,
    })
}

pub fn command_spec(executable: &TargetPath, arguments: &[String]) -> CommandSpec {
    CommandSpec {
        executable: executable.clone(),
        arguments: arguments.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_identity_uses_target_path_identity() {
        let target = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        let a = CommandSpec {
            executable: TargetPath::new(&target, r"C:\Apps\Acme.exe").unwrap(),
            arguments: vec!["--run".to_owned()],
        };
        let b = CommandSpec {
            executable: TargetPath::new(&target, r"c:/apps/acme.exe").unwrap(),
            arguments: vec!["--run".to_owned()],
        };
        let c = CommandSpec {
            executable: b.executable.clone(),
            arguments: vec!["--other".to_owned()],
        };
        assert!(commands_match(&a, &b));
        assert!(!commands_match(&a, &c));
    }
}

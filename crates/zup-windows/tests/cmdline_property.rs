use std::path::Path;

use proptest::prelude::*;
use zup_windows::{format_command_line, parse_command_line, split_command_line};

fn check(line: &str) {
    let (executable, arguments) = parse_command_line(line);
    let mut split = split_command_line(line);
    let expected = if split.is_empty() {
        None
    } else {
        Some(split.remove(0))
    };
    assert_eq!(
        executable, expected,
        "parse and split disagree about the executable: `{line}`"
    );
    assert_eq!(
        arguments, split,
        "parse and split disagree about the arguments: `{line}`"
    );
    if line.trim().is_empty() {
        assert!(
            executable.is_none(),
            "an empty command line produced an executable"
        );
    }

    let formatted = format_command_line(
        Path::new(executable.as_deref().unwrap_or("zup")),
        &arguments,
    );
    let (again_exe, again_args) = parse_command_line(&formatted);
    if executable.is_some() {
        assert_eq!(
            again_exe.as_deref(),
            executable.as_deref(),
            "formatting and re-parsing changed the executable: `{line}` -> `{formatted}`"
        );
    }
    assert_eq!(
        again_args, arguments,
        "formatting and re-parsing changed the arguments: `{line}` -> `{formatted}`"
    );

    let target = zup_core::TargetTriple::parse("x86_64-pc-windows-msvc").expect("a target");
    let Ok(spec) = zup_windows::command_spec_from_command_line(&formatted, &target) else {
        return;
    };
    let again = zup_windows::command_spec_from_command_line(&formatted, &target)
        .expect("the same line parses");
    assert!(
        zup_windows::commands_match(&spec, &again),
        "a command does not match itself: `{formatted}`"
    );
    let rebuilt = format_command_line(Path::new(&spec.executable.to_string()), &spec.arguments);
    let rebuilt = zup_windows::command_spec_from_command_line(&rebuilt, &target)
        .unwrap_or_else(|error| panic!("a re-formatted spec is not usable: {error}"));
    assert!(
        zup_windows::commands_match(&spec, &rebuilt),
        "formatting a spec and reading it back produced a different command"
    );
}

#[test]
fn unusual_whitespace_in_an_argument_survives_the_round_trip() {
    for separator in ['\r', '\u{c}', '\u{b}', '\u{a0}', '\u{2028}', '\u{3000}'] {
        let argument = format!("C:\\Program Files\\Acme{separator}setup.exe");
        let line = format_command_line(Path::new("zup.exe"), std::slice::from_ref(&argument));
        let (executable, arguments) = parse_command_line(&line);
        assert_eq!(
            executable.as_deref(),
            Some("zup.exe"),
            "the executable changed for a U+{:04X} separator: `{line}`",
            separator as u32
        );
        assert_eq!(
            arguments,
            vec![argument],
            "an argument containing U+{:04X} was quoted wrongly: `{line}`",
            separator as u32
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {

        cases: 2048,
        max_shrink_iters: 8192,
        ..ProptestConfig::default()
    })]

    #[test]
    fn a_command_line_holds_the_property(line in ".{0,200}") {
        check(&line);
    }

    #[test]
    fn formatting_and_parsing_are_inverse(
        exe in prop::collection::vec(any::<char>(), 0..24),
        args in prop::collection::vec(prop::collection::vec(any::<char>(), 0..24), 0..6),
    ) {
        let exe: String = exe.into_iter().filter(|c| !c.is_control()).collect();
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.into_iter().filter(|c| !c.is_control()).collect())
            .collect();
        let line = zup_windows::format_command_line(Path::new(&exe), &args);
        let (parsed_exe, parsed_args) = zup_windows::parse_command_line(&line);
        assert_eq!(parsed_exe.as_deref(), Some(exe.as_str()), "{line}");
        assert_eq!(parsed_args, args, "{line}");
    }
}

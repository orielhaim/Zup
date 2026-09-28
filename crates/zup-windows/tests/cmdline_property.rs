//! The command line must hold under generated arguments.
//!
//! This parser is the one in the product defined by another implementation -
//! `CommandLineToArgvW` in `shell32` - and the installer hands it
//! user-controlled strings, so a disagreement is not a wrong answer, it is the
//! wrong *files*.
//!
//! The property is the round trip the product actually relies on: a plan writes a
//! command line, a later process parses it, and the two must name the same
//! executable and the same arguments. An argument that survives formatting but
//! not parsing is a path that resolves somewhere else.
//!
//! Two directions, because one is not enough:
//!
//! - **parse → format → parse** is what the property states, and it is what found
//!   the bug this file exists for: `quote_arg` listed the whitespace characters it
//!   knew about while `split_command_line` split on all of them, so an argument
//!   containing a carriage return formatted unquoted and read back as two.
//! - **format → parse** generated from the *arguments* rather than from a line,
//!   because the inputs that break the pair are ones a line cannot express
//!   without already being right: a trailing backslash, an empty string, a quote
//!   in the middle.
//!
//! It lives in this crate's test suite because a command line is a Windows
//! concept, and the portable-boundary gate is right to refuse a portable crate
//! that branches on the build host.

use std::path::Path;

use proptest::prelude::*;
use zup_windows::{format_command_line, parse_command_line, split_command_line};

/// The property, stated over one line of text.
fn check(line: &str) {
    // Parsing never invents a token: `parse` is `split` minus the first one, and
    // the two are separate implementations that have to agree.
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

    // Format what was parsed and parse it again. A second round that changes
    // anything means the pair is not idempotent, which is how a command line
    // drifts between the process that wrote it and the one that reads it.
    //
    // A line with no executable is compared on its arguments alone: the formatter
    // needs a program to name, and the placeholder it is handed is not the one
    // that was absent.
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

    // And the spec comparison, which is the second reader: a plan records the
    // command it ran, and a later process asks whether the command it would run
    // now is the one already running.
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

/// The generators below cover every case a hand-written test would think of, so
/// the only list left is the one the corpus cannot reach.
///
/// `formatting_and_parsing_are_inverse` filters control characters out of the
/// arguments it generates, because a control character in a `CreateProcessW`
/// argument is a caller bug rather than a quoting rule. That leaves the
/// whitespace separators unguarded, and they are exactly the bug this file
/// exists for: `quote_arg` once quoted on `' '`, `\t`, `\n` and `\v` while
/// `split_command_line` split on `char::is_whitespace()`, so a carriage return,
/// a form feed or a non-breaking space formatted **unquoted** and read back as
/// two arguments - a path that resolves somewhere else.
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
        // More cases than the document properties, because the quoting rules have
        // a large input space and a small defect region: whitespace runs,
        // backslash runs, and quote placement.
        cases: 2048,
        max_shrink_iters: 8192,
        ..ProptestConfig::default()
    })]

    /// Checked over *text*, not bytes: the parser's input is a string, and a byte
    /// corpus spends most of its cases on inputs `from_utf8` throws away.
    #[test]
    fn a_command_line_holds_the_property(line in ".{0,200}") {
        check(&line);
    }

    /// The other direction, generated from the arguments.
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

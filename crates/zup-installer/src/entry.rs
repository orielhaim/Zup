//! The three entry points every setup runtime shares.
//!
//! A template binary is a file people copy, and a file people copy accumulates
//! divergent copies of whatever it contained. So each of the three is three lines
//! long and everything they have in common - the error report, the graphical
//! error dialog, the exit code - is decided here, once.

use std::process::ExitCode;

use zup_core::Frontend;

/// Run as the graphical frontend.
pub fn gui() -> ExitCode {
    match run(Frontend::Gui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // A windowed program has no console to write to, and a person who
            // double-clicked a setup file that failed needs to be told why. The
            // two cases that reach here are the two a person caused: a double
            // click, and an uninstall confirmation. An invocation with arguments
            // came from something that reads stderr.
            if was_launched_interactively() {
                show_error(&error.to_string());
            }
            ExitCode::from(crate::process_exit_code(&error))
        }
    }
}

/// Run as the console frontend.
pub fn console() -> ExitCode {
    match run(Frontend::Console) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(crate::process_exit_code(&error))
        }
    }
}

/// Run as the headless frontend.
pub fn headless() -> ExitCode {
    match run(Frontend::Headless) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(crate::process_exit_code(&error))
        }
    }
}

fn run(frontend: Frontend) -> miette::Result<()> {
    crate::run(frontend)
}

/// Whether this process was started by a person rather than by a script.
///
/// No arguments and no console-attached parent is the signature of a double
/// click. The uninstall confirmation is the other case: Apps & Features starts it
/// with arguments, and a person is watching, and a silent failure there looks
/// exactly like a broken uninstall.
fn was_launched_interactively() -> bool {
    let arguments = std::env::args_os().skip(1);
    let mut arguments = arguments.peekable();
    if arguments.peek().is_none() {
        return true;
    }
    arguments.any(|argument| argument == "__uninstall_runner")
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::PCWSTR;

    let message = message.encode_utf16().chain([0]).collect::<Vec<_>>();
    let title = "Application setup"
        .encode_utf16()
        .chain([0])
        .collect::<Vec<_>>();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_ICONERROR | MB_OK,
        );
    }
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}

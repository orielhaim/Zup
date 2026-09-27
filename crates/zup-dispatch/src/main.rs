//! The windowed universal dispatcher.
//!
//! A universal artifact is a user-facing installer, so it has one launcher
//! experience: a window. This binary is that launcher. It is built for 32-bit
//! Windows so one image starts on every machine a universal artifact serves, and
//! it selects the variant from the artifact index rather than from its own
//! architecture, which is exactly why it must not be.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    zup_dispatch::main_with(None, None)
}

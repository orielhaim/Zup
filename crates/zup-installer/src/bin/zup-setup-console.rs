//! The console setup runtime.
//!
//! This file is a *template*. `zup build` embeds a copy of it with one target's
//! plan compiled in and renames the result to the application's own installer
//! name, so nothing here should mention the Cargo package it was built as.

fn main() -> std::process::ExitCode {
    zup_installer::entry::console()
}

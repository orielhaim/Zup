//! The headless setup runtime.
//!
//! This file is a *template*. `zup build` embeds a copy of it with one target's
//! plan compiled in and renames the result to the application's own installer
//! name, so nothing here should mention the Cargo package it was built as.
//!
//! Headless exists for a deployment pipeline, so it never opens a window and never
//! asks a question. Everything it can do is on the command line.

fn main() -> std::process::ExitCode {
    zup_installer::entry::headless()
}

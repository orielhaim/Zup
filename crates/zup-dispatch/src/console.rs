//! The console universal dispatcher.
//!
//! Same launcher, terminal experience. A universal artifact is one launcher
//! experience, so an artifact whose variants are console installers is composed
//! from this template and one whose variants are windowed installers is composed
//! from the other.

fn main() -> std::process::ExitCode {
    zup_dispatch::main_with(None, None)
}

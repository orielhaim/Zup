//! Headless exists for a deployment pipeline, so it never opens a window and never

fn main() -> std::process::ExitCode {
    zup_installer::run::headless()
}

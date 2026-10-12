use std::process::ExitCode;

use zup_core::Frontend;

pub fn gui() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        eprintln!("the graphical installer is not supported on Linux in this phase");
        ExitCode::from(3)
    }
    #[cfg(not(target_os = "linux"))]
    match run_frontend(Frontend::Gui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if was_launched_interactively() {
                show_error(&error.to_string());
            }
            ExitCode::from(crate::process_exit_code(&error))
        }
    }
}

pub fn console() -> ExitCode {
    match run_frontend(Frontend::Console) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(crate::process_exit_code(&error))
        }
    }
}

pub fn headless() -> ExitCode {
    match run_frontend(Frontend::Headless) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(crate::process_exit_code(&error))
        }
    }
}

fn run_frontend(frontend: Frontend) -> miette::Result<()> {
    crate::run(frontend)
}

#[cfg(not(target_os = "linux"))]
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

#[cfg(all(not(target_os = "linux"), not(windows)))]
fn show_error(message: &str) {
    eprintln!("{message}");
}

use zup_presentation::OutputFormat;

#[derive(Debug, Clone, Copy)]
pub struct RuntimeContext {
    pub frontend: Frontend,
    pub output: OutputFormat,
}

impl RuntimeContext {
    pub fn new(frontend: Frontend) -> Self {
        Self {
            frontend,
            output: OutputFormat::Human,
        }
    }

    pub fn with_output(mut self, output: OutputFormat) -> Self {
        self.output = output;
        self
    }

    pub fn is_live_console(self) -> bool {
        self.frontend == Frontend::Console && console_is_a_terminal()
    }
}

pub fn console_is_a_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

// SECURITY: a launched process never holds an elevated handle; worker bootstrap enforces it.
#[cfg(windows)]
pub fn run_worker(bootstrap: &str) -> miette::Result<()> {
    let bootstrap = zup_windows::parse_bootstrap(bootstrap)
        .map_err(|error| miette::miette!("worker bootstrap rejected: {error}"))?;
    if bootstrap.expected_parent_pid == 0 {
        return Err(miette::miette!(
            "worker bootstrap rejected: zero parent pid"
        ));
    }
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("worker runtime: {error}"))?;
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio
        .block_on(zup_windows::run_worker(bootstrap, cancel))
        .map(|_| ())
        .map_err(|error| miette::miette!("worker failed: {error}"))
}

fn main() -> std::process::ExitCode {
    // Diagnostics are read by people who copy identifiers out of them — a target
    // triple, a digest, a path — and the default 80-column handler breaks those
    // across lines at hyphens. There is no terminal to wrap for in a build log,
    // so the width is unbounded.
    // Setting the hook twice is a programming error, not a runtime condition, and
    // the default is only a fallback this call replaces.
    let _ = miette::set_hook(Box::new(|_| {
        Box::new(miette::MietteHandlerOpts::new().width(usize::MAX).build())
    }));
    match zup::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:?}");
            std::process::ExitCode::from(zup::process_exit_code(&error))
        }
    }
}

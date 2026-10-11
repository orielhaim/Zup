fn main() -> std::process::ExitCode {
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

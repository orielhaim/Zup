fn main() -> std::process::ExitCode {
    match zup::run_as(zup_core::Frontend::Headless) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(zup::process_exit_code(&error))
        }
    }
}

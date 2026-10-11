fn main() -> std::process::ExitCode {
    let (options, state_root) = match zup_dispatch::options_from(std::env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => return zup_dispatch::usage_error(&error),
    };
    let options = zup_dispatch::Options {
        state_root,
        ..options
    };
    zup_dispatch::main_with(None, options)
}

fn main() -> std::process::ExitCode {
    let (options, state_root) = zup_dispatch::options_from(std::env::args_os().skip(1));
    let options = zup_dispatch::Options {
        state_root,
        ..options
    };
    zup_dispatch::main_with(None, options)
}

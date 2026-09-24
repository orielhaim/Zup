#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    if let Err(error) = zup::run() {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        let direct_launch = arguments.is_empty();
        let uninstall_ui_launch = arguments.first().is_some_and(|argument| {
            argument == "__uninstall_runner" && arguments.iter().any(|item| item == "--ui")
        });
        if direct_launch || uninstall_ui_launch {
            #[cfg(windows)]
            show_error(&error.to_string());
            #[cfg(not(windows))]
            eprintln!("{error:?}");
        }
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
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

//! What the user sees.
//!
//! The dispatcher is a launcher, so it says very little: which variant it chose,
//! whether that choice is native, and what went wrong if something did. A
//! windowed launcher has no terminal, so an error becomes a message box rather
//! than text nobody will read.

use zup_artifact::{ArtifactIndex, Compatibility};
use zup_windows::Selection;

/// Print or show the selected variant.
pub fn describe(selection: &Selection, index: &ArtifactIndex) {
    println!(
        "{} {} → {} ({})",
        index.artifact.application.name,
        index.artifact.application.version,
        selection.id,
        selection_word(selection.compatibility),
    );
}

fn selection_word(compatibility: Compatibility) -> &'static str {
    match compatibility {
        Compatibility::Native => "native",
        Compatibility::Emulated => "compatibility mode",
        Compatibility::Unsupported => "unsupported",
    }
}

/// Report a failure.
///
/// The windowed dispatcher has no console to print to, so it shows a window; the
/// console one shows the same text on the terminal and the same window. Both say
/// the same thing, because one artifact is one launcher experience.
pub fn error(message: &str) {
    eprintln!("{message}");
    show(message);
}

#[cfg(windows)]
fn show(message: &str) {
    use std::ffi::c_void;
    use windows_link::link;
    type Handle = *mut c_void;
    type Wchar = u16;
    link!("user32.dll" "system" fn MessageBoxW(owner: Handle, text: *const Wchar, caption: *const Wchar, kind: u32) -> i32);
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_OK: u32 = 0x0000_0000;
    let text: Vec<Wchar> = message.encode_utf16().chain(Some(0)).collect();
    let caption: Vec<Wchar> = "Application setup".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR | MB_OK,
        );
    }
}

#[cfg(not(windows))]
fn show(_message: &str) {}

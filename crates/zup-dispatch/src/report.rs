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
    eprintln!(
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
pub fn error(message: &str, graphical: bool) {
    eprintln!("{message}");
    if graphical {
        message_box(message, 0x10);
    }
}

pub fn choose_scope(graphical: bool) -> Result<zup_core::SelectedScope, String> {
    use zup_core::SelectedScope;
    if graphical {
        return match message_box(
            "Install for everyone on this computer?\n\nYes: all users, administrator approval required.\nNo: only your account.",
            0x03 | 0x20 | 0x100,
        ) {
            6 => Ok(SelectedScope::Machine),
            7 => Ok(SelectedScope::User),
            _ => Err("scope selection cancelled".into()),
        };
    }
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        return Err(zup_core::ScopeSelectionError::ChoiceRequired.to_string());
    }
    eprint!("Install for your account [user] or everyone [machine]? ");
    std::io::stderr()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut choice = String::new();
    std::io::stdin()
        .read_line(&mut choice)
        .map_err(|error| error.to_string())?;
    match choice.trim().to_ascii_lowercase().as_str() {
        "user" | "u" => Ok(SelectedScope::User),
        "machine" | "m" => Ok(SelectedScope::Machine),
        _ => Err(zup_core::ScopeSelectionError::ChoiceRequired.to_string()),
    }
}

#[cfg(windows)]
fn message_box(message: &str, style: u32) -> i32 {
    use std::ffi::c_void;
    use windows_link::link;
    type Handle = *mut c_void;
    type Wchar = u16;
    link!("user32.dll" "system" fn MessageBoxW(owner: Handle, text: *const Wchar, caption: *const Wchar, kind: u32) -> i32);
    let text: Vec<Wchar> = message.encode_utf16().chain(Some(0)).collect();
    let caption: Vec<Wchar> = "Application setup".encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), style) }
}

#[cfg(not(windows))]
fn message_box(_message: &str, _style: u32) -> i32 {
    0
}

/// What this launcher accepts.
///
/// Printed on `--help` and nowhere else. A lifecycle verb plus locations and
/// reporting flags: everything the staged runtime needs travels verbatim, and
/// the runtime re-validates it. There is no option that names a release, a
/// digest, or a URL, because nothing this program does would be safer for it.
pub fn print_usage() {
    println!("zup installer launcher");
    println!();
    println!("  [install | modify | repair | update | uninstall] [options]");
    println!("  install is the default; installing a newer artifact upgrades");
    println!();
    println!("  --scope <user|machine>   whose installation to affect");
    println!("  --state-root <dir>       where this machine records its installations");
    println!("  --source <dir>           a local release tree to read before the network");
    println!("  --non-interactive, --yes never ask a question");
    println!("  --force-files            repair files even when nothing drifted");
    println!("  --enable <id>, --disable <id>");
    println!("                           components to include or skip");
    println!("  --install-directory <dir>");
    println!("                           where to install the application");
    println!("  --output <human|json|jsonl>");
    println!("                           report progress as JSONL on stdout");
    println!("  --help                   this text");
}

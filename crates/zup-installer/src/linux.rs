use std::path::PathBuf;

#[cfg(feature = "gui")]
use clap::Parser;
use zup_core::Frontend;

#[cfg(feature = "gui")]
use crate::terminal::Scope;

#[cfg(feature = "gui")]
#[derive(Debug, Parser)]
#[command(name = "setup", version)]
struct Cli {
    #[arg(long, global = true)]
    scope: Option<Scope>,

    /// scope only inside the machine program tree: an override never widens
    #[arg(long, global = true, value_name = "PATH")]
    install_dir: Option<PathBuf>,

    #[arg(long, global = true)]
    state_root: Option<PathBuf>,
}

pub fn command() -> clap::Command {
    crate::terminal::command()
}

pub fn run(frontend: Frontend) -> miette::Result<()> {
    if let Some((session, client_pid, socket_path)) = worker_session_args() {
        return run_worker(session, client_pid, socket_path);
    }
    if frontend == Frontend::Gui
        && !std::env::args().any(|argument| {
            matches!(
                argument.as_str(),
                "recover" | "update" | "--help" | "-h" | "--version" | "-V"
            )
        })
    {
        #[cfg(feature = "gui")]
        return run_gui();
        #[cfg(not(feature = "gui"))]
        return Err(miette::miette!(
            "this setup runtime has no graphical frontend"
        ));
    }
    crate::terminal::run_cli(
        zup_linux::LinuxSessionHost::open(PathBuf::new()),
        frontend,
        crate::terminal::parse(),
    )
}

#[cfg(feature = "gui")]
fn run_gui() -> miette::Result<()> {
    if let Some((session, client_pid, socket_path)) = worker_session_args() {
        return run_worker(session, client_pid, socket_path);
    }
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let graphical = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
        || session_type.eq_ignore_ascii_case("wayland")
        || session_type.eq_ignore_ascii_case("x11");
    if !graphical {
        return Err(miette::miette!(
            "the graphical installer needs a Wayland or X11 session, and none is available \
             (no WAYLAND_DISPLAY or DISPLAY). Run the console installer instead, or start a \
             graphical session first"
        ));
    }
    let cli = Cli::parse();
    if cli.install_dir.is_some() || cli.state_root.is_some() {
        return Err(miette::miette!(
            "the graphical installer takes no operation arguments; choose the operation in the window"
        ));
    }
    let executable =
        std::env::current_exe().map_err(|error| miette::miette!("executable: {error}"))?;
    let scope = cli.scope.map(Into::into);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("session runtime: {error}"))?
        .block_on(zup_session::run(
            zup_linux::LinuxSessionHost::open(executable.clone()),
            &executable,
            zup_session::SessionLaunch {
                scope,
                state_root: None,
                interactive: true,
                ..Default::default()
            },
        ))
}

fn worker_session_args() -> Option<(String, u32, PathBuf)> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "__privileged-worker" {
            let mut session = None;
            let mut client_pid = None;
            let mut socket_path = None;
            while let Some(flag) = args.next() {
                if flag == "--session" {
                    session = args.next();
                } else if flag == "--client-pid" {
                    client_pid = args.next().and_then(|pid| pid.parse::<u32>().ok());
                } else if flag == "--socket-path" {
                    socket_path = args.next().map(PathBuf::from);
                }
            }
            return session.map(|session| {
                (
                    session,
                    client_pid.unwrap_or(0),
                    socket_path.unwrap_or_default(),
                )
            });
        }
    }
    None
}

fn run_worker(session: String, client_pid: u32, socket_path: PathBuf) -> miette::Result<()> {
    let session: zup_protocol::SessionId = session
        .parse()
        .map_err(|_| miette::miette!("the worker serves one session, named by uuid"))?;
    match zup_linux::run_worker_mode(session, client_pid, &socket_path) {
        Ok(outcome) => {
            println!("worker: {outcome}");
            Ok(())
        }
        Err(error) => Err(miette::miette!("{error}")),
    }
}

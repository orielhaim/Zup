use wasmtime::component::{Accessor, HasData, Linker};
use wasmtime_wasi::cli::{WasiCliCtxView, WasiCliView};
use wasmtime_wasi::p3::bindings::clocks::{monotonic_clock, system_clock, types};

use crate::runtime::Sandbox;

pub(crate) fn add_to_linker(linker: &mut Linker<Sandbox>) -> wasmtime::Result<()> {
    wasmtime_wasi::p3::cli::add_to_linker(linker)?;
    monotonic_clock::add_to_linker::<_, Sandbox>(linker, |state| state)?;
    system_clock::add_to_linker::<_, Sandbox>(linker, |state| state)?;
    Ok(())
}

impl WasiCliView for Sandbox {
    fn cli(&mut self) -> WasiCliCtxView<'_> {
        WasiCliCtxView {
            ctx: &mut self.cli,
            table: &mut self.table,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("clock access is forbidden in installation planners")]
pub(crate) struct ClockDenied;

impl types::Host for Sandbox {}

impl HasData for Sandbox {
    type Data<'a> = &'a mut Self;
}

impl system_clock::Host for Sandbox {
    fn now(&mut self) -> wasmtime::Result<system_clock::Instant> {
        Err(ClockDenied.into())
    }

    fn get_resolution(&mut self) -> wasmtime::Result<types::Duration> {
        Err(ClockDenied.into())
    }
}

impl monotonic_clock::Host for Sandbox {
    fn now(&mut self) -> wasmtime::Result<monotonic_clock::Mark> {
        Err(ClockDenied.into())
    }

    fn get_resolution(&mut self) -> wasmtime::Result<types::Duration> {
        Err(ClockDenied.into())
    }
}

impl<T> monotonic_clock::HostWithStore<T> for Sandbox {
    async fn wait_until(_: &Accessor<T, Self>, _: monotonic_clock::Mark) -> wasmtime::Result<()> {
        Err(ClockDenied.into())
    }

    async fn wait_for(_: &Accessor<T, Self>, _: types::Duration) -> wasmtime::Result<()> {
        Err(ClockDenied.into())
    }
}

pub(crate) fn allows_import(name: &str) -> bool {
    matches!(
        name,
        "wasi:cli/environment@0.3.0"
            | "wasi:cli/exit@0.3.0"
            | "wasi:cli/types@0.3.0"
            | "wasi:cli/stdin@0.3.0"
            | "wasi:cli/stdout@0.3.0"
            | "wasi:cli/stderr@0.3.0"
            | "wasi:cli/terminal-input@0.3.0"
            | "wasi:cli/terminal-output@0.3.0"
            | "wasi:cli/terminal-stdin@0.3.0"
            | "wasi:cli/terminal-stdout@0.3.0"
            | "wasi:cli/terminal-stderr@0.3.0"
            | "wasi:clocks/types@0.3.0"
            | "wasi:clocks/monotonic-clock@0.3.0"
            | "wasi:clocks/system-clock@0.3.0"
    )
}

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;
use wasmtime::{Engine, ResourceLimiter, StoreLimits, StoreLimitsBuilder, Trap};

use crate::{
    EPOCH_DEADLINE_TICKS, INVOCATION_DEADLINE_MILLIS, MAX_FUEL_PER_CALL, MAX_INSTANCES,
    MAX_MEMORY_BYTES, MAX_MEMORY_COUNT, MAX_PLAN_OUTPUT_BYTES, MAX_TABLE_COUNT, MAX_TABLE_ELEMENTS,
};

const MAX_ERROR_MESSAGE_BYTES: usize = 512;
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(5);
const INVOCATION_DEADLINE: Duration = Duration::from_millis(INVOCATION_DEADLINE_MILLIS);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvocationError {
    #[error("plugin invocation setup failed: {message}")]
    Setup { message: String },
    #[error("plugin trapped: {message}")]
    Trap { message: String },
    #[error("plugin exhausted its fuel budget")]
    FuelExhausted,
    #[error("plugin exceeded a sandbox memory limit")]
    MemoryLimit,
    #[error("plugin invocation was cancelled")]
    Cancelled,
    #[error("plugin invocation timed out")]
    Timeout,
    #[error("plugin returned {actual} output bytes, limit is {limit}")]
    OutputLimit { actual: u64, limit: u64 },
    #[error("plugin returned {actual} {resource}, limit is {limit}")]
    ResourceLimit {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("plugin returned invalid output: {message}")]
    InvalidOutput { message: String },
    #[error("plugin invocation failed internally: {message}")]
    Internal { message: String },
}

impl InvocationError {
    pub(crate) fn setup(error: wasmtime::Error) -> Self {
        Self::Setup {
            message: sanitize_error(error.to_string()),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: sanitize_error(message.into()),
        }
    }
}

pub(crate) fn sandbox_store(engine: &Engine) -> wasmtime::Result<wasmtime::Store<Sandbox>> {
    let mut store = wasmtime::Store::new(engine, Sandbox::new());
    store.limiter(|limits| limits);
    store.set_fuel(MAX_FUEL_PER_CALL)?;
    store.set_hostcall_fuel(MAX_PLAN_OUTPUT_BYTES);
    #[cfg(target_has_atomic = "64")]
    {
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        store.epoch_deadline_trap();
    }
    Ok(store)
}

pub(crate) struct Sandbox {
    limits: StoreLimits,
    pub(crate) growth_rejected: bool,
    pub(crate) cli: wasmtime_wasi::cli::WasiCliCtx,
    pub(crate) table: wasmtime::component::ResourceTable,
}

impl Sandbox {
    fn new() -> Self {
        let mut table = wasmtime::component::ResourceTable::new();
        table.set_max_capacity(crate::MAX_WASI_RESOURCES);
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY_BYTES)
                .table_elements(MAX_TABLE_ELEMENTS as usize)
                .memories(MAX_MEMORY_COUNT)
                .tables(MAX_TABLE_COUNT)
                .instances(MAX_INSTANCES)
                .trap_on_grow_failure(true)
                .build(),
            growth_rejected: false,
            cli: wasmtime_wasi::cli::WasiCliCtx::default(),
            table,
        }
    }
}

impl ResourceLimiter for Sandbox {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let result = self.limits.memory_growing(current, desired, maximum);
        if !matches!(result, Ok(true)) {
            self.growth_rejected = true;
        }
        result
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.growth_rejected = true;
        self.limits.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let result = self.limits.table_growing(current, desired, maximum);
        if !matches!(result, Ok(true)) {
            self.growth_rejected = true;
        }
        result
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.growth_rejected = true;
        self.limits.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.limits.instances()
    }

    fn tables(&self) -> usize {
        self.limits.tables()
    }

    fn memories(&self) -> usize {
        self.limits.memories()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchdogCause {
    None,
    Cancelled,
    Timeout,
    Internal,
}

pub(crate) fn run_with_watchdog<T: Send>(
    engine: &Engine,
    cancellation: &(dyn Fn() -> bool + Send + Sync),
    operation: impl std::future::Future<Output = wasmtime::Result<T>> + Send,
) -> Result<T, InvocationError> {
    let query_cancellation = || match catch_unwind(AssertUnwindSafe(cancellation)) {
        Ok(true) => WatchdogCause::Cancelled,
        Ok(false) => WatchdogCause::None,
        Err(_) => WatchdogCause::Internal,
    };
    match query_cancellation() {
        WatchdogCause::Cancelled => return Err(InvocationError::Cancelled),
        WatchdogCause::Internal => {
            return Err(InvocationError::internal("cancellation query panicked"));
        }
        _ => {}
    }

    let deadline = Instant::now() + INVOCATION_DEADLINE;
    let (interrupt, mut interrupted) = tokio::sync::watch::channel(WatchdogCause::None);
    let (completed, completion) = mpsc::channel();
    let (result, cause) = thread::scope(|scope| {
        // The synchronous planning API can also be called inside a Tokio runtime.
        let worker = thread::Builder::new()
            .name("zup-plugin".to_owned())
            .spawn_scoped(scope, move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()?;
                    runtime.block_on(async {
                        tokio::select! {
                            result = operation => result,
                            _ = interrupted.changed() => Err(Trap::Interrupt.into()),
                        }
                    })
                }));
                let _ = completed.send(());
                result
            })
            .map_err(|error| {
                InvocationError::internal(format!("could not start plugin: {error}"))
            })?;
        let cause = loop {
            let cause = query_cancellation();
            if cause != WatchdogCause::None {
                break cause;
            }
            let now = Instant::now();
            if now >= deadline {
                break WatchdogCause::Timeout;
            }
            match completion.recv_timeout((deadline - now).min(WATCHDOG_POLL_INTERVAL)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break WatchdogCause::None,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        if cause != WatchdogCause::None {
            interrupt_engine(engine);
            let _ = interrupt.send(cause);
        }
        let result = worker
            .join()
            .map_err(|_| InvocationError::internal("plugin thread panicked"))?;
        Ok((result, cause))
    })?;
    let result = result.map_err(|_| InvocationError::internal("plugin invocation panicked"))?;
    match cause {
        WatchdogCause::Cancelled => return Err(InvocationError::Cancelled),
        WatchdogCause::Timeout => return Err(InvocationError::Timeout),
        WatchdogCause::Internal => {
            return Err(InvocationError::internal("cancellation query panicked"));
        }
        WatchdogCause::None => {}
    }
    result.map_err(|error| classify_error(&error, query_cancellation()))
}

#[cfg(target_has_atomic = "64")]
fn interrupt_engine(engine: &Engine) {
    engine.increment_epoch();
}

#[cfg(not(target_has_atomic = "64"))]
fn interrupt_engine(_: &Engine) {}

fn classify_error(error: &wasmtime::Error, cause: WatchdogCause) -> InvocationError {
    if let Some(trap) = error.root_cause().downcast_ref::<Trap>()
        && matches!(trap, Trap::OutOfFuel)
    {
        return InvocationError::FuelExhausted;
    }
    match cause {
        WatchdogCause::Cancelled => return InvocationError::Cancelled,
        WatchdogCause::Timeout => return InvocationError::Timeout,
        WatchdogCause::Internal => {
            return InvocationError::internal("invocation watchdog failed");
        }
        WatchdogCause::None => {}
    }
    if let Some(trap) = error.root_cause().downcast_ref::<Trap>() {
        return match trap {
            Trap::OutOfFuel => InvocationError::FuelExhausted,
            Trap::Interrupt => InvocationError::Timeout,
            Trap::MemoryOutOfBounds
            | Trap::StringOutOfBounds
            | Trap::ListOutOfBounds
            | Trap::InvalidChar
            | Trap::InvalidDiscriminant
            | Trap::UnalignedPointer
            | Trap::ArrayOutOfBounds
            | Trap::NullReference
            | Trap::CastFailure => InvocationError::InvalidOutput {
                message: sanitize_error(trap.to_string()),
            },
            _ => InvocationError::Trap {
                message: sanitize_error(trap.to_string()),
            },
        };
    }
    InvocationError::Trap {
        message: sanitize_error(error.to_string()),
    }
}

pub(crate) fn sanitize_error(message: String) -> String {
    let mut output = String::with_capacity(MAX_ERROR_MESSAGE_BYTES);
    for character in message.chars() {
        if output.len() >= MAX_ERROR_MESSAGE_BYTES {
            break;
        }
        if character.is_control() {
            output.push(' ');
        } else if character.len_utf8() <= MAX_ERROR_MESSAGE_BYTES - output.len() {
            output.push(character);
        } else {
            break;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn watchdog_stops_a_synchronous_call_at_the_deadline() {
        let engine = Engine::default();
        let result = run_with_watchdog(&engine, &|| false, async {
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        });
        assert_eq!(result, Err(InvocationError::Timeout));
    }

    #[rstest::rstest]
    #[case::deadline(false)]
    #[case::cancelled(true)]
    fn suspended_execution_is_interrupted_and_dropped(#[case] cancel: bool) {
        let engine = Engine::default();
        let started = Instant::now();
        let dropped = std::sync::atomic::AtomicBool::new(false);
        struct OnDrop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for OnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let pending = async {
            let _guard = OnDrop(&dropped);
            std::future::pending::<()>().await;
            Ok(())
        };
        let result = run_with_watchdog(
            &engine,
            &|| cancel && started.elapsed() > Duration::from_millis(20),
            pending,
        );
        assert_eq!(
            result,
            Err(if cancel {
                InvocationError::Cancelled
            } else {
                InvocationError::Timeout
            })
        );
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn interrupt_is_classified_from_control_state() {
        let engine = Engine::default();
        let timeout = run_with_watchdog(&engine, &|| false, async {
            Err::<(), _>(Trap::Interrupt.into())
        });
        assert_eq!(timeout, Err(InvocationError::Timeout));

        let calls = AtomicUsize::new(0);
        let cancellation = || calls.fetch_add(1, Ordering::Relaxed) > 0;
        let cancelled = run_with_watchdog(&engine, &cancellation, async {
            Err::<(), _>(Trap::Interrupt.into())
        });
        assert_eq!(cancelled, Err(InvocationError::Cancelled));
    }

    #[test]
    fn a_trap_is_reported_as_its_own_failure() {
        let classify = |trap: Trap| classify_error(&trap.into(), WatchdogCause::None);
        assert_eq!(
            classify(Trap::OutOfFuel),
            InvocationError::FuelExhausted,
            "a guest that burned its fuel budget is reported as having done that"
        );
        assert!(
            matches!(
                classify(Trap::MemoryOutOfBounds),
                InvocationError::InvalidOutput { .. }
            ),
            "and a guest that walked off the end of its memory is reported as that"
        );
        assert!(
            matches!(classify(Trap::Interrupt), InvocationError::Timeout),
            "an interrupt with no cancellation pending is a timeout"
        );
        assert!(
            matches!(
                classify(Trap::UnreachableCodeReached),
                InvocationError::Trap { .. }
            ),
            "and anything else keeps the trap, so the guest is told what it did"
        );
    }
}

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, Ordering};
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

pub(crate) fn sandbox_store(engine: &Engine) -> wasmtime::Result<wasmtime::Store<SandboxLimits>> {
    let mut store = wasmtime::Store::new(engine, SandboxLimits::new());
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

#[derive(Debug)]
pub(crate) struct SandboxLimits {
    limits: StoreLimits,
    pub(crate) growth_rejected: bool,
}

impl SandboxLimits {
    fn new() -> Self {
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
        }
    }
}

impl ResourceLimiter for SandboxLimits {
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

pub(crate) fn run_with_watchdog<T>(
    engine: &Engine,
    cancellation: &(dyn Fn() -> bool + Send + Sync),
    operation: impl FnOnce() -> wasmtime::Result<T>,
) -> Result<T, InvocationError> {
    match catch_unwind(AssertUnwindSafe(cancellation)) {
        Ok(true) => return Err(InvocationError::Cancelled),
        Ok(false) => {}
        Err(_) => return Err(InvocationError::internal("cancellation query panicked")),
    }

    let cause = AtomicU8::new(0);
    let (stop_sender, stop_receiver) = mpsc::channel();
    let deadline = Instant::now() + INVOCATION_DEADLINE;
    let operation_result = thread::scope(|scope| {
        let handle = thread::Builder::new()
            .name("zup-plugin-watchdog".to_owned())
            .spawn_scoped(scope, || {
                watchdog(engine, cancellation, &cause, stop_receiver, deadline)
            })
            .map_err(|error| {
                InvocationError::internal(format!("could not start invocation watchdog: {error}"))
            })?;
        let result = catch_unwind(AssertUnwindSafe(operation));
        let _ = stop_sender.send(());
        handle
            .join()
            .map_err(|_| InvocationError::internal("invocation watchdog panicked"))?;
        Ok(result)
    })?;
    let operation_result = match operation_result {
        Ok(result) => result,
        Err(_) => return Err(InvocationError::internal("plugin invocation panicked")),
    };
    let cause = match cause.load(Ordering::Acquire) {
        1 => WatchdogCause::Cancelled,
        2 => WatchdogCause::Timeout,
        3 => WatchdogCause::Internal,
        _ => WatchdogCause::None,
    };
    match operation_result {
        Ok(value) => match cause {
            WatchdogCause::Cancelled => Err(InvocationError::Cancelled),
            WatchdogCause::Timeout => Err(InvocationError::Timeout),
            WatchdogCause::Internal => Err(InvocationError::internal("invocation watchdog failed")),
            WatchdogCause::None => Ok(value),
        },
        Err(error) => {
            let cause =
                if matches!(cause, WatchdogCause::None) && cancellation_requested(cancellation) {
                    WatchdogCause::Cancelled
                } else {
                    cause
                };
            Err(classify_error(&error, cause))
        }
    }
}

fn cancellation_requested(cancellation: &(dyn Fn() -> bool + Send + Sync)) -> bool {
    catch_unwind(AssertUnwindSafe(cancellation)).is_ok_and(|value| value)
}

fn watchdog(
    engine: &Engine,
    cancellation: &(dyn Fn() -> bool + Send + Sync),
    cause: &AtomicU8,
    stop_receiver: mpsc::Receiver<()>,
    deadline: Instant,
) {
    loop {
        if stop_receiver.try_recv().is_ok() {
            return;
        }
        let cancelled = match catch_unwind(AssertUnwindSafe(cancellation)) {
            Ok(value) => value,
            Err(_) => {
                set_cause(cause, 3);
                interrupt_engine(engine);
                return;
            }
        };
        if cancelled {
            set_cause(cause, 1);
            interrupt_engine(engine);
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            set_cause(cause, 2);
            interrupt_engine(engine);
            return;
        }
        let wait = (deadline - now).min(WATCHDOG_POLL_INTERVAL);
        if stop_receiver.recv_timeout(wait).is_ok() {
            return;
        }
    }
}

fn set_cause(cause: &AtomicU8, value: u8) {
    let _ = cause.compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
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
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn watchdog_stops_a_synchronous_call_at_the_deadline() {
        let engine = Engine::default();
        let result = run_with_watchdog(&engine, &|| false, || {
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        });
        assert_eq!(result, Err(InvocationError::Timeout));
    }

    #[test]
    fn interrupt_is_classified_from_control_state() {
        let engine = Engine::default();
        let timeout =
            run_with_watchdog(&engine, &|| false, || Err::<(), _>(Trap::Interrupt.into()));
        assert_eq!(timeout, Err(InvocationError::Timeout));

        let calls = AtomicUsize::new(0);
        let cancellation = || calls.fetch_add(1, Ordering::Relaxed) > 0;
        let cancelled = run_with_watchdog(&engine, &cancellation, || {
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

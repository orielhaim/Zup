use std::sync::Arc;

use gpui_kit::{App, AppContext, AsyncApp, Entity};
use zup_preset_protocol::{Action, Snapshot};

#[derive(Debug, Clone, Default)]
pub struct SessionState {
    snapshot: Option<Snapshot>,
    connected: bool,
    closed: Option<String>,
}

impl SessionState {
    pub fn connected(snapshot: Snapshot) -> Self {
        Self {
            snapshot: Some(snapshot),
            connected: true,
            closed: None,
        }
    }

    pub fn disconnected() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn closed(&self) -> Option<&str> {
        self.closed.as_deref()
    }

    pub fn replace(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
        self.connected = true;
        self.closed = None;
    }
}

pub trait ActionSender: Send + Sync + 'static {
    fn send(&self, action: Action);
}

impl<F> ActionSender for F
where
    F: Fn(Action) + Send + Sync + 'static,
{
    fn send(&self, action: Action) {
        self(action)
    }
}

pub struct Session {
    state: Entity<SessionState>,
    sender: Arc<dyn ActionSender>,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            sender: Arc::clone(&self.sender),
        }
    }
}

impl Session {
    pub fn open(cx: &mut App, sender: impl ActionSender) -> Self {
        Self::with_state(cx.new(|_| SessionState::default()), sender)
    }

    pub fn with_state(state: Entity<SessionState>, sender: impl ActionSender) -> Self {
        Self {
            state,
            sender: Arc::new(sender),
        }
    }

    pub fn state(&self) -> Entity<SessionState> {
        self.state.clone()
    }

    pub fn send(&self, action: Action) {
        self.sender.send(action);
    }

    pub(crate) fn publish(&self, cx: &mut App, snapshot: Snapshot) {
        self.apply(cx, |state| {
            state.snapshot = Some(snapshot);
            state.connected = true;
            state.closed = None;
        });
    }

    pub(crate) fn publish_from_task(&self, cx: &mut AsyncApp, snapshot: Snapshot) {
        let state = self.state.clone();
        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.snapshot = Some(snapshot);
                state.connected = true;
                state.closed = None;
                cx.notify();
            });
        });
    }

    fn apply(&self, cx: &mut App, change: impl FnOnce(&mut SessionState)) {
        let state = self.state.clone();
        state.update(cx, |state, cx| {
            change(state);
            cx.notify();
        });
    }

    pub(crate) fn refresh(&self, cx: &mut AsyncApp) {
        let state = self.state.clone();
        cx.update(|cx| {
            state.update(cx, |_, cx| cx.notify());
        });
    }

    pub(crate) fn disconnect(&self, cx: &mut AsyncApp, reason: Option<String>) {
        let state = self.state.clone();
        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.connected = false;
                state.closed = reason;
                cx.notify();
            });
        });
    }
}

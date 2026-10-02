//! The session a preset draws from.
//!
//! One object, and the whole of a preset's reach into the installer: the
//! current state in, typed intent out. The state is a GPUI entity so a preset
//! observes it and re-renders, because a preset that had to poll, or to
//! reconcile its own copy against the host's, would be a second source of truth
//! wearing a window.

use std::sync::Arc;

use gpui_kit::{App, AppContext, AsyncApp, Entity};
use zup_ui_protocol::{UiAction, UiSnapshot};

/// The installer's current state, as far as a preset is concerned.
///
/// The session owns exactly one of these and replaces it whole: a snapshot is
/// the complete state, so a preset that reconnects, starts late, or missed a
/// message all render correctly from what they were handed.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    snapshot: Option<UiSnapshot>,
    connected: bool,
    /// Why the session ended, when it did.
    closed: Option<String>,
}

impl SessionState {
    /// A session the host has already published state into.
    ///
    /// A preset's own tests need this: they are testing what a window does with
    /// a state, and standing up a host to produce one would test the transport
    /// instead. It is the same state the host would have published, so a window
    /// cannot tell the difference - which is the point.
    pub fn connected(snapshot: UiSnapshot) -> Self {
        Self {
            snapshot: Some(snapshot),
            connected: true,
            closed: None,
        }
    }

    /// A session the host never answered.
    pub fn disconnected() -> Self {
        Self::default()
    }

    /// The current state, or `None` before the host's first snapshot.
    pub fn snapshot(&self) -> Option<&UiSnapshot> {
        self.snapshot.as_ref()
    }

    /// Whether the host is still on the other end.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Why the session ended, if it has.
    pub fn closed(&self) -> Option<&str> {
        self.closed.as_deref()
    }

    /// Replace the snapshot, as a host publishing one would.
    ///
    /// For a preset's own tests and previews: the window still renders only
    /// what it is handed, and a caller notifies the entity afterwards.
    pub fn replace(&mut self, snapshot: UiSnapshot) {
        self.snapshot = Some(snapshot);
        self.connected = true;
        self.closed = None;
    }
}

/// Where an action goes once a preset has sent it.
///
/// A trait rather than a concrete channel so a preset's tests can hand a
/// session a list of actions and read it back, without standing up a host.
pub trait ActionSender: Send + Sync + 'static {
    fn send(&self, action: UiAction);
}

impl<F> ActionSender for F
where
    F: Fn(UiAction) + Send + Sync + 'static,
{
    fn send(&self, action: UiAction) {
        self(action)
    }
}

/// A preset's connection to the installer host.
///
/// Observing the session is how a preset learns something changed, and
/// [`UiSession::send`] is how it asks for anything:
///
/// ```no_run
/// # use gpui_kit::{Context, IntoElement, ParentElement, Render, Window, div};
/// # use zup_ui_sdk::prelude::*;
/// # struct View { session: UiSession }
/// # impl View {
/// #     fn install(&self) { self.session.send(UiAction::Install); }
/// # }
/// # impl Render for View {
/// #     fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
/// #         div().child(self.session.state().read(cx).is_connected().to_string())
/// #     }
/// # }
/// ```
pub struct UiSession {
    state: Entity<SessionState>,
    sender: Arc<dyn ActionSender>,
}

impl Clone for UiSession {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            sender: Arc::clone(&self.sender),
        }
    }
}

impl UiSession {
    /// Open a session whose actions go to `sender`.
    pub fn open(cx: &mut App, sender: impl ActionSender) -> Self {
        Self::with_state(cx.new(|_| SessionState::default()), sender)
    }

    /// Open a session around a state a caller already owns.
    ///
    /// The runtime and a test both need to seed a session rather than wait for a
    /// first snapshot, and both would otherwise reach into a private field to
    /// do it.
    pub fn with_state(state: Entity<SessionState>, sender: impl ActionSender) -> Self {
        Self {
            state,
            sender: Arc::new(sender),
        }
    }

    /// The current state, as something a view can observe.
    pub fn state(&self) -> Entity<SessionState> {
        self.state.clone()
    }

    /// Ask the host to do something.
    pub fn send(&self, action: UiAction) {
        self.sender.send(action);
    }

    /// Replace the state, and tell everything observing the session.
    ///
    /// Takes the application itself, because this is the one place a caller is
    /// *already holding* it: the SDK seeds the first snapshot from inside
    /// `Application::run`, which borrows the application for the whole of its
    /// callback. Reaching it again through an async handle borrows it twice and
    /// panics, which is why this is not a single method taking one kind of
    /// context. A caller that is not already holding it uses
    /// [`UiSession::publish_from_task`], which is the same change made the way a
    /// spawned task has to make it.
    ///
    /// Called by the SDK as the host publishes. A preset does not call it: a
    /// preset that could set the state would be a preset that could contradict
    /// the machine, which is the one thing a preset must never be.
    pub(crate) fn publish(&self, cx: &mut App, snapshot: UiSnapshot) {
        self.apply(cx, |state| {
            state.snapshot = Some(snapshot);
            state.connected = true;
            state.closed = None;
        });
    }

    /// The same change, made from a spawned task.
    ///
    /// A task does not hold the application - that is what it is for - so it has
    /// to borrow one, and this is how it does. The state transition itself is
    /// [`UiSession::apply`], so there is one of it rather than one per context.
    pub(crate) fn publish_from_task(&self, cx: &mut AsyncApp, snapshot: UiSnapshot) {
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

    /// The one place the session's state changes, for any caller that has the
    /// application in hand.
    fn apply(&self, cx: &mut App, change: impl FnOnce(&mut SessionState)) {
        let state = self.state.clone();
        state.update(cx, |state, cx| {
            change(state);
            cx.notify();
        });
    }

    /// The configuration changed: the state a view draws is unchanged, but the
    /// settings and assets it draws with are not.
    pub(crate) fn refresh(&self, cx: &mut AsyncApp) {
        let state = self.state.clone();
        cx.update(|cx| {
            state.update(cx, |_, cx| cx.notify());
        });
    }

    /// Note that the host is gone, and why.
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

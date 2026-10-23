use std::sync::Arc;

use crate::AppEvent;

/// Where tasks off the loop thread post [`AppEvent`]s: the winit
/// `EventLoopProxy` in a window, a channel in tests.
#[derive(Clone)]
pub(crate) struct EventSink(Arc<dyn Fn(AppEvent) -> bool + Send + Sync>);

impl EventSink {
    pub(crate) fn new(post: impl Fn(AppEvent) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(post))
    }

    pub(crate) fn from_proxy(proxy: winit::event_loop::EventLoopProxy<AppEvent>) -> Self {
        Self::new(move |event| proxy.send_event(event).is_ok())
    }

    /// `false` once the loop is gone; a task that loops on posting stops
    /// there.
    pub(crate) fn send(&self, event: AppEvent) -> bool {
        (self.0)(event)
    }
}

//! Event stream plumbing for the TUI.
//!
//! - [`EventBroker`] holds the shared crossterm stream so multiple callers reuse the same
//!   input source and can drop/recreate it on pause/resume without rebuilding consumers.
//! - [`TuiEventStream`] wraps a draw event subscription plus the shared [`EventBroker`] and maps crossterm
//!   events into [`TuiEvent`]. The broker also owns the tmux size monitor; its samples
//!   wake the draw subscription and become resize events before rendering.
//! - [`EventSource`] abstracts the underlying event producer; the real implementation is
//!   [`CrosstermEventSource`] and tests can swap in [`FakeEventSource`].
//!
//! The motivation for dropping/recreating the crossterm event stream is to enable the TUI to fully relinquish stdin.
//! If the stream is not dropped, it will continue to read from stdin even if it is not actively being polled
//! (due to how crossterm's EventStream is implemented), potentially stealing input from other processes reading stdin,
//! like terminal text editors. This race can cause missed input or capturing terminal query responses (for example, OSC palette/size queries)
//! that the other process expects to read. Stopping polling, instead of dropping the stream, is only sufficient when the
//! pause happens before the stream enters a pending state; otherwise the crossterm reader thread may keep reading
//! from stdin, so the safer approach is to drop and recreate the event stream when we need to hand off the terminal.
//!
//! See https://ratatui.rs/recipes/apps/spawn-vim/ and https://www.reddit.com/r/rust/comments/1f3o33u/myterious_crossterm_input_after_running_vim for more details.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use crossterm::event::ColorReport;
use crossterm::event::ColorScheme;
use crossterm::event::Event;
use crossterm::style::Color;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::TuiEvent;
use super::size_monitor::SizeMonitor;

/// Result type produced by an event source.
pub type EventResult = std::io::Result<Event>;

/// Delay before re-querying a palette reply that contradicted its mode 2031 report.
const SCHEME_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Abstraction over a source of terminal events. Allows swapping in a fake for tests.
/// Value in production is [`CrosstermEventSource`].
pub trait EventSource: Send + 'static {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventResult>>;

    /// Ask the terminal to report its default colors as [`ColorReport`] events.
    ///
    /// Implementations must only write the query: the replies arrive through [`Self::poll_next`],
    /// so waiting here would stall the input loop and consume typed keys.
    fn request_default_colors(&mut self) {}
}

/// Shared crossterm input state for all [`TuiEventStream`] instances. A single crossterm EventStream
/// is reused so all streams still see the same input source.
///
/// This intermediate layer enables dropping/recreating the underlying EventStream (pause/resume) without rebuilding consumers.
pub struct EventBroker<S: EventSource = CrosstermEventSource> {
    state: Mutex<EventBrokerState<S>>,
    resume_events_tx: watch::Sender<()>,
    pub(super) size_monitor: Option<SizeMonitor>,
}

/// Tracks state of underlying [`EventSource`].
enum EventBrokerState<S: EventSource> {
    Paused,     // Underlying event source (i.e., crossterm EventStream) dropped
    Start,      // A new event source will be created on next poll
    Running(S), // Event source is currently running
}

impl<S: EventSource + Default> EventBrokerState<S> {
    /// Return the running event source, starting it if needed; None when paused.
    fn active_event_source_mut(&mut self) -> Option<&mut S> {
        match self {
            EventBrokerState::Paused => None,
            EventBrokerState::Start => {
                *self = EventBrokerState::Running(S::default());
                match self {
                    EventBrokerState::Running(events) => Some(events),
                    EventBrokerState::Paused | EventBrokerState::Start => unreachable!(),
                }
            }
            EventBrokerState::Running(events) => Some(events),
        }
    }
}

impl<S: EventSource + Default> EventBroker<S> {
    pub fn new() -> Self {
        let (resume_events_tx, _resume_events_rx) = watch::channel(());
        Self {
            state: Mutex::new(EventBrokerState::Start),
            resume_events_tx,
            size_monitor: None,
        }
    }

    /// Drop the underlying event source
    pub fn pause_events(&self) {
        if let Some(monitor) = &self.size_monitor {
            monitor.set_active(/*active*/ false);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = EventBrokerState::Paused;
    }

    /// Create a new instance of the underlying event source
    pub fn resume_events(&self) {
        if let Some(monitor) = &self.size_monitor {
            monitor.set_active(/*active*/ true);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = EventBrokerState::Start;
        let _ = self.resume_events_tx.send(());
    }

    /// Subscribe to a notification that fires whenever [`Self::resume_events`] is called.
    ///
    /// This is used to wake `poll_crossterm_event` when it is paused and waiting for the
    /// underlying crossterm stream to be recreated.
    pub fn resume_events_rx(&self) -> watch::Receiver<()> {
        self.resume_events_tx.subscribe()
    }

    /// Ask the terminal for its default colors unless another program owns terminal input.
    pub fn request_default_colors(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(events) = state.active_event_source_mut() {
            events.request_default_colors();
        }
    }
}

/// Real crossterm-backed event source.
pub struct CrosstermEventSource(pub crossterm::event::EventStream);

impl Default for CrosstermEventSource {
    fn default() -> Self {
        Self(crossterm::event::EventStream::new())
    }
}

impl EventSource for CrosstermEventSource {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventResult>> {
        // Crossterm's Windows backend expects Win32 input records. If VT input is inherited or
        // restored by another console client, navigation keys arrive as literal escape bytes.
        #[cfg(windows)]
        let _ = super::windows_console::ensure_input_record_mode();

        let result = Pin::new(&mut self.get_mut().0).poll_next(cx);

        // EventStream starts its blocking reader before returning Pending, so reassert the mode
        // after that transition as well.
        #[cfg(windows)]
        if result.is_pending() {
            let _ = super::windows_console::ensure_input_record_mode();
        }

        result
    }

    #[cfg(unix)]
    fn request_default_colors(&mut self) {
        use std::io::Write;

        let mut stdout = std::io::stdout();
        if let Err(err) = stdout
            .write_all(super::terminal_colors::DEFAULT_COLOR_QUERY)
            .and_then(|()| stdout.flush())
        {
            tracing::debug!(error = %err, "failed to request terminal default colors");
        }
    }
}

/// TuiEventStream is a struct for reading TUI events (draws and user input).
/// Each instance has its own draw subscription (the draw channel is broadcast, so
/// multiple receivers are fine), while crossterm input is funneled through a
/// single shared [`EventBroker`] because crossterm uses a global stdin reader and
/// does not support fan-out. Multiple TuiEventStream instances can exist during the app lifetime
/// (for nested or sequential screens), but only one should be polled at a time,
/// otherwise one instance can consume ("steal") input events and the other will miss them.
pub struct TuiEventStream<S: EventSource + Default + Unpin = CrosstermEventSource> {
    broker: Arc<EventBroker<S>>,
    draw_stream: BroadcastStream<()>,
    resume_stream: WatchStream<()>,
    terminal_focused: Arc<AtomicBool>,
    poll_draw_first: bool,
    /// OSC 10/11 replies of the current query round, applied together once both arrive.
    pending_foreground: Option<(u8, u8, u8)>,
    pending_background: Option<(u8, u8, u8)>,
    /// Scheme announced by the mode 2031 report that started the current query round.
    reported_scheme: Option<ColorScheme>,
    /// One delayed re-query after a reply contradicted the reported scheme.
    scheme_retry: Option<Pin<Box<tokio::time::Sleep>>>,
    #[cfg(unix)]
    suspend_context: crate::tui::job_control::SuspendContext,
    #[cfg(unix)]
    alt_screen_active: Arc<AtomicBool>,
}

impl<S: EventSource + Default + Unpin> TuiEventStream<S> {
    pub fn new(
        broker: Arc<EventBroker<S>>,
        draw_rx: broadcast::Receiver<()>,
        terminal_focused: Arc<AtomicBool>,
        #[cfg(unix)] suspend_context: crate::tui::job_control::SuspendContext,
        #[cfg(unix)] alt_screen_active: Arc<AtomicBool>,
    ) -> Self {
        let resume_stream = WatchStream::from_changes(broker.resume_events_rx());
        Self {
            broker,
            draw_stream: BroadcastStream::new(draw_rx),
            resume_stream,
            terminal_focused,
            poll_draw_first: false,
            pending_foreground: None,
            pending_background: None,
            reported_scheme: None,
            scheme_retry: None,
            #[cfg(unix)]
            suspend_context,
            #[cfg(unix)]
            alt_screen_active,
        }
    }

    /// Poll the shared crossterm stream for the next mapped `TuiEvent`.
    ///
    /// This skips events we don't use and keeps polling until it yields
    /// a mapped event, hits `Pending`, or sees EOF/error. When the broker is paused, it drops
    /// the underlying stream and returns `Pending` to fully release stdin.
    pub fn poll_crossterm_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        // Some crossterm events map to None (e.g. mouse); loop so we keep polling
        // until we return a mapped event, hit Pending, or see EOF/error.
        loop {
            let poll_result = {
                let mut state = self
                    .broker
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let events = match state.active_event_source_mut() {
                    Some(events) => events,
                    None => {
                        drop(state);
                        // Poll resume_stream so resume_events wakes a stream paused here
                        match Pin::new(&mut self.resume_stream).poll_next(cx) {
                            Poll::Ready(Some(())) => continue,
                            Poll::Ready(None) => return Poll::Ready(None),
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                };
                match Pin::new(events).poll_next(cx) {
                    Poll::Ready(Some(Ok(event))) => Some(event),
                    Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                        *state = EventBrokerState::Start;
                        return Poll::Ready(None);
                    }
                    Poll::Pending => {
                        drop(state);
                        // Poll resume_stream so resume_events can wake us even while waiting on stdin
                        match Pin::new(&mut self.resume_stream).poll_next(cx) {
                            Poll::Ready(Some(())) => continue,
                            Poll::Ready(None) => return Poll::Ready(None),
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                }
            };

            if let Some(mapped) = poll_result.and_then(|event| self.map_crossterm_event(event)) {
                return Poll::Ready(Some(mapped));
            }
        }
    }

    /// Poll the draw broadcast stream for the next draw event. Draw events are used to trigger a redraw of the TUI.
    pub fn poll_draw_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        match Pin::new(&mut self.draw_stream).poll_next(cx) {
            Poll::Ready(Some(Ok(())))
            | Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                let event = self
                    .broker
                    .size_monitor
                    .as_ref()
                    .and_then(SizeMonitor::take_resize)
                    .map_or(TuiEvent::Draw, TuiEvent::Resize);
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Map a crossterm event to a [`TuiEvent`], preserving mouse coordinates and modifiers.
    fn map_crossterm_event(&mut self, event: Event) -> Option<TuiEvent> {
        match event {
            Event::Key(key_event) => {
                #[cfg(unix)]
                if crate::tui::job_control::SUSPEND_KEY.is_press(key_event) {
                    self.broker.pause_events();
                    let suspend_result = self.suspend_context.suspend(&self.alt_screen_active);
                    self.broker.resume_events();
                    // Mode 2031 was off while suspended, so re-read the palette.
                    self.request_default_colors();
                    if let Err(err) = suspend_result {
                        tracing::warn!(
                            event = "tui_suspend_failed",
                            error = %err,
                            "failed to suspend TUI process"
                        );
                    }
                    return Some(TuiEvent::Resume);
                }
                Some(TuiEvent::Key(key_event))
            }
            Event::Resize(width, height) => {
                let size = ratatui::layout::Size { width, height };
                if let Some(monitor) = &self.broker.size_monitor {
                    monitor.observe(size);
                }
                Some(TuiEvent::Resize(size))
            }
            Event::Paste(pasted) => Some(TuiEvent::Paste(pasted)),
            Event::FocusGained => {
                self.terminal_focused.store(true, Ordering::Relaxed);
                // Terminals without mode 2031 still refresh the palette on focus.
                self.request_default_colors();
                Some(TuiEvent::FocusGained)
            }
            Event::FocusLost => {
                self.terminal_focused.store(false, Ordering::Relaxed);
                Some(TuiEvent::FocusLost)
            }
            Event::Mouse(mouse) => Some(TuiEvent::Mouse(mouse)),
            Event::ColorReport(ColorReport::ColorScheme(scheme)) => {
                self.request_default_colors();
                self.reported_scheme = Some(scheme);
                None
            }
            Event::ColorReport(ColorReport::ForegroundColor(Color::Rgb { r, g, b })) => {
                self.pending_foreground = Some((r, g, b));
                self.apply_reported_colors()
            }
            Event::ColorReport(ColorReport::BackgroundColor(Color::Rgb { r, g, b })) => {
                self.pending_background = Some((r, g, b));
                self.apply_reported_colors()
            }
            Event::ColorReport(
                ColorReport::ForegroundColor(_) | ColorReport::BackgroundColor(_),
            ) => None,
        }
    }

    /// Start a palette query round, dropping unpaired replies and any retry of an earlier round.
    fn request_default_colors(&mut self) {
        (self.pending_foreground, self.pending_background) = (None, None);
        (self.reported_scheme, self.scheme_retry) = (None, None);
        self.broker.request_default_colors();
    }

    /// Apply the reported palette once both replies of a query round arrived, in either order.
    fn apply_reported_colors(&mut self) -> Option<TuiEvent> {
        let (Some(fg), Some(bg)) = (self.pending_foreground, self.pending_background) else {
            return None;
        };
        (self.pending_foreground, self.pending_background) = (None, None);
        // Multiplexers such as herdr relay the report to an unfocused pane but answer its query
        // with the colors from before the switch; ask once more after they caught up.
        if let Some(scheme) = self.reported_scheme.take()
            && (scheme == ColorScheme::Light) != crate::color::is_light(bg)
        {
            self.scheme_retry = Some(Box::pin(tokio::time::sleep(SCHEME_RETRY_DELAY)));
        }
        let colors = crate::terminal_probe::DefaultColors { fg, bg };
        crate::terminal_palette::update_default_colors(colors).then_some(TuiEvent::Draw)
    }

    /// Send the pending scheme retry query once its delay has elapsed.
    fn poll_scheme_retry(&mut self, cx: &mut Context<'_>) {
        if let Some(retry) = self.scheme_retry.as_mut()
            && retry.as_mut().poll(cx).is_ready()
        {
            self.request_default_colors();
        }
    }
}

impl<S: EventSource + Default + Unpin> Unpin for TuiEventStream<S> {}

impl<S: EventSource + Default + Unpin> Stream for TuiEventStream<S> {
    type Item = TuiEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // approximate fairness + no starvation via round-robin.
        let draw_first = self.poll_draw_first;
        self.poll_draw_first = !self.poll_draw_first;

        if draw_first {
            if let Poll::Ready(event) = self.poll_draw_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
                return Poll::Ready(event);
            }
        } else {
            if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_draw_event(cx) {
                return Poll::Ready(event);
            }
        }

        // Last, so a retry armed while mapping this poll's replies registers its timer.
        self.poll_scheme_retry(cx);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::ColorScheme;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use crossterm::event::MouseEvent;
    use crossterm::event::MouseEventKind;
    use pretty_assertions::assert_eq;
    use std::task::Context;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use tokio_stream::StreamExt;

    /// Simple fake event source for tests; feed events via the handle.
    struct FakeEventSource {
        rx: mpsc::UnboundedReceiver<EventResult>,
        tx: mpsc::UnboundedSender<EventResult>,
        color_requests: usize,
    }

    struct FakeEventSourceHandle {
        broker: Arc<EventBroker<FakeEventSource>>,
    }

    impl FakeEventSource {
        fn new() -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                rx,
                tx,
                color_requests: 0,
            }
        }
    }

    impl Default for FakeEventSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeEventSourceHandle {
        fn new(broker: Arc<EventBroker<FakeEventSource>>) -> Self {
            Self { broker }
        }

        fn send(&self, event: EventResult) {
            let mut state = self
                .broker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(source) = state.active_event_source_mut() else {
                return;
            };
            let _ = source.tx.send(event);
        }

        fn color_requests(&self) -> usize {
            match &*self
                .broker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                EventBrokerState::Running(source) => source.color_requests,
                EventBrokerState::Paused | EventBrokerState::Start => 0,
            }
        }
    }

    impl EventSource for FakeEventSource {
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventResult>> {
            Pin::new(&mut self.get_mut().rx).poll_recv(cx)
        }

        fn request_default_colors(&mut self) {
            self.color_requests += 1;
        }
    }

    fn make_stream(
        broker: Arc<EventBroker<FakeEventSource>>,
        draw_rx: broadcast::Receiver<()>,
        terminal_focused: Arc<AtomicBool>,
    ) -> TuiEventStream<FakeEventSource> {
        TuiEventStream::new(
            broker,
            draw_rx,
            terminal_focused,
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        )
    }

    type SetupState = (
        Arc<EventBroker<FakeEventSource>>,
        FakeEventSourceHandle,
        broadcast::Sender<()>,
        broadcast::Receiver<()>,
        Arc<AtomicBool>,
    );

    fn setup() -> SetupState {
        let source = FakeEventSource::new();
        let broker = Arc::new(EventBroker::new());
        *broker.state.lock().unwrap() = EventBrokerState::Running(source);
        let handle = FakeEventSourceHandle::new(broker.clone());

        let (draw_tx, draw_rx) = broadcast::channel(1);
        let terminal_focused = Arc::new(AtomicBool::new(true));
        (broker, handle, draw_tx, draw_rx, terminal_focused)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mouse_events_preserve_coordinates_and_do_not_consume_the_next_key() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let expected = MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 123,
            row: 42,
            modifiers: KeyModifiers::SHIFT,
        };
        handle.send(Ok(Event::Mouse(expected)));
        match stream.next().await {
            Some(TuiEvent::Mouse(actual)) => assert_eq!(actual, expected),
            other => panic!("expected mouse event, got {other:?}"),
        }
        let expected = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        handle.send(Ok(Event::Key(expected)));
        match stream.next().await {
            Some(TuiEvent::Key(actual)) => assert_eq!(actual, expected),
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_lost_is_forwarded_and_updates_terminal_state() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused.clone());

        handle.send(Ok(Event::FocusLost));

        assert!(matches!(stream.next().await, Some(TuiEvent::FocusLost)));
        assert!(!terminal_focused.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_lost_consumed_by_startup_remains_visible_to_the_tui() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
        tui.terminal_focused = terminal_focused.clone();
        let mut startup_events = make_stream(broker, draw_rx, terminal_focused);

        assert!(tui.is_terminal_focused());
        handle.send(Ok(Event::FocusLost));

        assert!(matches!(
            startup_events.next().await,
            Some(TuiEvent::FocusLost)
        ));
        assert!(!tui.is_terminal_focused());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_gained_preserves_already_queued_key() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        terminal_focused.store(false, Ordering::Relaxed);
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused.clone());
        let expected_key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);

        handle.send(Ok(Event::FocusGained));
        handle.send(Ok(Event::Key(expected_key)));

        assert!(matches!(stream.next().await, Some(TuiEvent::FocusGained)));
        assert!(terminal_focused.load(Ordering::Relaxed));
        assert_eq!(handle.color_requests(), 1);
        assert!(matches!(
            &*broker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            EventBrokerState::Running(_)
        ));

        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("focus handling discarded an already queued key");

        match next {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected queued key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn draw_and_key_events_yield_both() {
        let (broker, handle, draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        let expected_key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let _ = draw_tx.send(());
        handle.send(Ok(Event::Key(expected_key)));

        let first = stream.next().await.unwrap();
        let second = stream.next().await.unwrap();

        let mut saw_draw = false;
        let mut saw_key = false;
        for event in [first, second] {
            match event {
                TuiEvent::Draw => {
                    saw_draw = true;
                }
                TuiEvent::Key(key) => {
                    assert_eq!(key, expected_key);
                    saw_key = true;
                }
                other => panic!("expected draw or key event, got {other:?}"),
            }
        }

        assert!(saw_draw && saw_key, "expected both draw and key events");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lagged_draw_maps_to_draw() {
        let (broker, _handle, draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx.resubscribe(), terminal_focused);

        // Fill channel to force Lagged on the receiver.
        let _ = draw_tx.send(());
        let _ = draw_tx.send(());

        let first = stream.next().await;
        assert!(matches!(first, Some(TuiEvent::Draw)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resize_event_maps_to_resize() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send(Ok(Event::Resize(80, 24)));

        let next = stream.next().await;
        assert!(matches!(
            next,
            Some(TuiEvent::Resize(ratatui::layout::Size {
                width: 80,
                height: 24
            }))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn error_or_eof_ends_stream() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send(Err(std::io::Error::other("boom")));

        let next = stream.next().await;
        assert!(next.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_wakes_paused_stream() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused);

        broker.pause_events();

        let task = tokio::spawn(async move { stream.next().await });
        tokio::task::yield_now().await;

        broker.resume_events();
        let expected_key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        handle.send(Ok(Event::Key(expected_key)));

        let event = timeout(Duration::from_millis(100), task)
            .await
            .expect("timed out waiting for resumed event")
            .expect("join failed");
        match event {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_wakes_pending_stream() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused);

        let task = tokio::spawn(async move { stream.next().await });
        tokio::task::yield_now().await;

        broker.pause_events();
        broker.resume_events();
        let expected_key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        handle.send(Ok(Event::Key(expected_key)));

        let event = timeout(Duration::from_millis(100), task)
            .await
            .expect("timed out waiting for resumed event")
            .expect("join failed");
        match event {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn color_scheme_reports_request_default_colors_between_keys() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let keys = ['a', 'b', 'c'].map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));

        handle.send(Ok(Event::Key(keys[0])));
        handle.send(Ok(Event::ColorReport(ColorReport::ColorScheme(
            ColorScheme::Dark,
        ))));
        handle.send(Ok(Event::Key(keys[1])));
        handle.send(Ok(Event::ColorReport(ColorReport::ColorScheme(
            ColorScheme::Light,
        ))));
        handle.send(Ok(Event::Key(keys[2])));

        let mut delivered = Vec::new();
        for _ in keys {
            match stream.next().await {
                Some(TuiEvent::Key(key)) => delivered.push(key),
                other => panic!("expected key event, got {other:?}"),
            }
        }
        assert_eq!((delivered, handle.color_requests()), (keys.to_vec(), 2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_color_replies_pair_in_either_order() {
        let (broker, _handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let events = crate::terminal_palette::with_test_default_colors(DARK, || {
            [
                fg(DARK),
                bg(DARK),
                bg(LIGHT),
                fg(LIGHT),
                fg(LIGHT),
                bg(LIGHT),
            ]
            .map(|event| format!("{:?}", stream.map_crossterm_event(event)))
        });

        assert_eq!(
            events,
            ["None", "None", "None", "Some(Draw)", "None", "Some(Draw)"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unpaired_color_reply_is_dropped_by_the_next_query_round() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let scheme = Event::ColorReport(ColorReport::ColorScheme(ColorScheme::Light));
        let events = crate::terminal_palette::with_test_default_colors(DARK, || {
            [fg(LIGHT), scheme, bg(LIGHT), fg(LIGHT)]
                .map(|event| format!("{:?}", stream.map_crossterm_event(event)))
        });

        assert_eq!(
            (events, handle.color_requests()),
            (["None", "None", "None", "Some(Draw)"].map(String::from), 1)
        );
    }

    #[test]
    fn stale_reply_to_a_color_scheme_report_is_retried_once() {
        // Stale replies repeat the cached palette, so no redraw event polls the stream again.
        let requests = crate::terminal_palette::with_test_default_colors(DARK, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
                let mut stream = make_stream(broker, draw_rx, terminal_focused);
                // Like the app loop, only the stream's own wakeups poll it again.
                let reader = tokio::spawn(async move { while stream.next().await.is_some() {} });
                let light = || Event::ColorReport(ColorReport::ColorScheme(ColorScheme::Light));
                let mut requests = Vec::new();
                for events in [
                    // A light report answered with dark colors gets one delayed re-query.
                    vec![light(), fg(DARK), bg(DARK)],
                    // The re-query's reply is never retried.
                    vec![fg(DARK), bg(DARK)],
                    // A reply matching the report is final.
                    vec![light(), fg(LIGHT), bg(LIGHT)],
                    // A newer report drops the pending re-query.
                    vec![light(), bg(DARK), fg(DARK), light()],
                ] {
                    for event in events {
                        handle.send(Ok(event));
                    }
                    tokio::time::sleep(Duration::from_secs(/*secs*/ 1)).await;
                    requests.push(handle.color_requests());
                }
                reader.abort();
                requests
            })
        });

        assert_eq!(requests, [2, 2, 3, 5]);
    }

    const DARK: crate::terminal_probe::DefaultColors = crate::terminal_probe::DefaultColors {
        fg: (238, 238, 238),
        bg: (17, 17, 17),
    };
    const LIGHT: crate::terminal_probe::DefaultColors = crate::terminal_probe::DefaultColors {
        fg: (17, 17, 17),
        bg: (250, 250, 250),
    };

    fn fg(colors: crate::terminal_probe::DefaultColors) -> Event {
        let (r, g, b) = colors.fg;
        Event::ColorReport(ColorReport::ForegroundColor(Color::Rgb { r, g, b }))
    }

    fn bg(colors: crate::terminal_probe::DefaultColors) -> Event {
        let (r, g, b) = colors.bg;
        Event::ColorReport(ColorReport::BackgroundColor(Color::Rgb { r, g, b }))
    }
}

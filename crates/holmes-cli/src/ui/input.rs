//! Single keyboard-input thread for the inline UI.
//!
//! Historically there were TWO crossterm readers: the idle event loop (`event::poll`
//! / `event::read`) and a per-turn interrupt watcher on a spawned task. Two readers
//! racing on stdin is fragile (an event consumed by one is lost to the other). This
//! module replaces both with ONE plain `std::thread` that owns stdin for the whole
//! UI lifetime and forwards events over an unbounded channel; the app — idle loop
//! and busy turn loop alike — is the only consumer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Events the app cares about. `Paste` only arrives when the terminal supports
/// bracketed paste (enabled in `TerminalGuard`); on terminals without it a paste
/// arrives as ordinary key events, which the app handles as before — no degradation,
/// just no paste-chip affordance. Everything else (focus, mouse) is dropped at the
/// source so the channel stays cheap to drain.
#[derive(Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    /// A bracketed paste: the full pasted text as ONE event (newlines intact).
    Paste(String),
}

/// Owning handle for the input thread. `rx` is polled by the app (idle loop and the
/// busy turn's `select!`); dropping the handle signals the thread to stop and joins
/// it. The thread also exits on its own if the channel closes or stdin errors.
pub struct InputThread {
    pub rx: UnboundedReceiver<InputEvent>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl InputThread {
    /// Spawn the reader thread. Requires raw mode to be enabled already (crossterm
    /// reads escape sequences byte-wise; without raw mode keys arrive line-buffered).
    pub fn spawn() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("holmes-input".into())
            .spawn(move || input_loop(tx, thread_stop))
            .expect("spawn input thread");
        Self {
            rx,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // The thread polls with a timeout, so the join always returns promptly.
            let _ = handle.join();
        }
    }
}

fn input_loop(tx: UnboundedSender<InputEvent>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        // Poll with a timeout so the stop flag is honored even when stdin is quiet.
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    let Some(ev) = filter_event(ev) else { continue };
                    // A closed channel means the app is gone — stop reading stdin.
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                Err(_) => return, // stdin broke; nothing useful left to do here
            },
            Ok(false) => {}
            Err(_) => return,
        }
    }
}

/// Keep only key presses/repeats (releases are a Kitty-protocol artifact the UI never
/// handles), bracketed-paste payloads, and resize notifications.
fn filter_event(event: Event) -> Option<InputEvent> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => Some(InputEvent::Key(key)),
        Event::Resize(w, h) => Some(InputEvent::Resize(w, h)),
        Event::Paste(text) => Some(InputEvent::Paste(text)),
        _ => None,
    }
}

/// Drain `first` plus everything already queued in `rx` into one batch. The app
/// processes the whole batch and redraws ONCE, so a keystroke/paste burst (or a
/// terminal replaying buffered input) costs a single frame instead of one per event.
pub fn drain_pending(rx: &mut UnboundedReceiver<InputEvent>, first: InputEvent) -> Vec<InputEvent> {
    let mut batch = vec![first];
    while let Ok(ev) = rx.try_recv() {
        batch.push(ev);
    }
    batch
}

/// Coalesces a stream of resize events (dragging a window edge emits one per mouse
/// step): each resize is noted, and the burst only counts as settled once no new
/// resize has arrived for `WINDOW`. The inline viewport auto-fits its width on the
/// next draw, so "handling" a resize is just redrawing — debouncing skips the
/// flickery intermediate frames during a drag.
pub struct ResizeDebounce {
    last: Option<Instant>,
}

impl ResizeDebounce {
    /// Quiet window after the last resize before redrawing.
    pub const WINDOW: Duration = Duration::from_millis(16);

    pub fn new() -> Self {
        Self { last: None }
    }

    /// Note a resize event at `now` (restarts the quiet window).
    pub fn mark(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// Forget any pending burst (call once the settled redraw has happened).
    pub fn clear(&mut self) {
        self.last = None;
    }

    /// Time until the current burst settles; `None` when no resize is pending.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.last
            .map(|t| Self::WINDOW.saturating_sub(now.saturating_duration_since(t)))
    }

    /// A burst is pending and its quiet window has fully elapsed.
    pub fn settled(&self, now: Instant) -> bool {
        self.remaining(now) == Some(Duration::ZERO)
    }
}

impl Default for ResizeDebounce {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    fn key(kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            kind,
        ))
    }

    #[test]
    fn releases_are_dropped_presses_and_repeats_forwarded() {
        assert!(filter_event(key(KeyEventKind::Release)).is_none());
        assert!(matches!(
            filter_event(key(KeyEventKind::Press)),
            Some(InputEvent::Key(_))
        ));
        assert!(matches!(
            filter_event(key(KeyEventKind::Repeat)),
            Some(InputEvent::Key(_))
        ));
        assert!(matches!(
            filter_event(Event::Resize(80, 24)),
            Some(InputEvent::Resize(80, 24))
        ));
        assert!(matches!(
            filter_event(Event::Paste("a\nb".into())),
            Some(InputEvent::Paste(_))
        ));
        assert!(filter_event(Event::FocusGained).is_none());
    }

    #[test]
    fn drain_pending_collects_everything_queued() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(InputEvent::Paste("a".into())).unwrap();
        tx.send(InputEvent::Paste("b".into())).unwrap();
        tx.send(InputEvent::Resize(80, 24)).unwrap();
        tx.send(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        let first = rx.try_recv().expect("first event");
        let batch = drain_pending(&mut rx, first);
        assert_eq!(batch.len(), 4, "first plus the three queued events");
        assert!(rx.try_recv().is_err(), "channel fully drained");
    }

    #[test]
    fn resize_debounce_settles_after_quiet_window() {
        let mut d = ResizeDebounce::new();
        let t0 = Instant::now();
        assert_eq!(d.remaining(t0), None, "nothing pending");
        assert!(!d.settled(t0));
        d.mark(t0);
        assert_eq!(d.remaining(t0), Some(ResizeDebounce::WINDOW));
        assert!(!d.settled(t0 + Duration::from_millis(15)));
        assert!(d.settled(t0 + Duration::from_millis(16)));
        // A fresh resize restarts the window.
        d.mark(t0 + Duration::from_millis(20));
        assert!(!d.settled(t0 + Duration::from_millis(35)));
        assert!(d.settled(t0 + Duration::from_millis(36)));
        d.clear();
        assert_eq!(d.remaining(t0 + Duration::from_millis(40)), None);
    }
}

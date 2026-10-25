//! The terminal's keyboard under the kitty keyboard protocol (ADR-441, amended 2026-10-25):
//! a held key as a native app has it.
//!
//! A legacy terminal sends a held key as a run of presses queued in the tty, so behind a slow
//! frame they back up and keep playing after the key came up. A terminal that speaks the
//! kitty protocol (kitty, foot, WezTerm, Ghostty, Alacritty 0.13+) can say which events are
//! repeats and when a key is released. `term-enter` asks for that when the terminal says it
//! can, and this module reads it:
//!
//! - **A repeat that is already stale is dropped.** Everything the tty has buffered is read
//!   ahead into `ahead`; a repeat whose key's next event in that input is another repeat or
//!   its release is superseded, so it never reaches the app ([`repeat_superseded`]). That is
//!   the terminal's version of the window's gate and withdraw: the backlog is gone the moment
//!   the key is up. A press is never dropped, and neither is typed text.
//! - **A release is `[:key-up press]`**, `press` the event its press delivered, found by the
//!   key, not its modifiers, so Ctrl or Shift let go first still names the chord.
//! - **The held key is known**, for `term-held-key` and the display's `:key-held?`.
//!
//! Without the protocol nothing here changes what a poll returns: no repeat or release
//! arrives, and nothing is ever held.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyboardEnhancementFlags, MouseEvent,
};

/// What `term-enter` asks a kitty-protocol terminal for. Disambiguation makes a chord a
/// `CSI … u` sequence (the only form that can carry an event type); event types add the
/// repeats and releases; alternate keys give a shifted chord its layout's glyph, so
/// Alt+Shift+`,` is still `:alt-<` as it is in a legacy terminal (without them the terminal
/// sends the unshifted `,` with Shift). Keys that type text are still sent as text, so plain
/// typing reads exactly as it did.
pub(super) const FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// How many events one poll reads ahead at most: a bound on the work, far above what a held
/// key buffers behind one slow frame.
const READ_AHEAD: usize = 256;

/// How many keys are remembered as down. A terminal that reports no release for some key
/// (kitty sends a text key's release only when every key is an escape code) would otherwise
/// grow the list for as long as the program runs.
const DOWN_LIMIT: usize = 16;

/// The key an event is about, whatever its modifiers: a character in lower case (a chord's
/// release with Shift already up sends the lower-case key) and Shift+Tab as Tab.
pub(super) fn key_identity(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(character) => {
            let mut lower = character.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(only), None) => KeyCode::Char(only),
                _ => code,
            }
        }
        KeyCode::BackTab => KeyCode::Tab,
        other => other,
    }
}

/// Whether a key is one that can be held: a bare modifier, a lock key or a media key is
/// delivered (or not) as before but never tracked.
fn trackable(code: KeyCode) -> bool {
    !matches!(
        code,
        KeyCode::Modifier(_)
            | KeyCode::Media(_)
            | KeyCode::Null
            | KeyCode::CapsLock
            | KeyCode::NumLock
            | KeyCode::ScrollLock
            | KeyCode::PrintScreen
            | KeyCode::KeypadBegin
    )
}

/// The coalescing rule: a repeat of `identity` is superseded when that key's next event in
/// the input already buffered behind it (`ahead`) is another repeat or its release. Only a
/// repeat is ever judged; a press later in the input does not supersede one (it is a fresh
/// keystroke, which is never dropped, and the repeat before it is real).
pub(super) fn repeat_superseded<'a>(
    identity: KeyCode,
    ahead: impl IntoIterator<Item = &'a Event>,
) -> bool {
    ahead
        .into_iter()
        .find_map(|event| match event {
            Event::Key(later) if key_identity(later.code) == identity => {
                Some(later.kind != KeyEventKind::Press)
            }
            _ => None,
        })
        .unwrap_or(false)
}

/// What a key event becomes for the app.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum KeyDelivery {
    /// A press, or a repeat let through: the key this event spells.
    Press(KeyEvent),
    /// A release: `[:key-up …]` of the key this press event spelled.
    Release(KeyEvent),
}

/// The keys down and the one held, folded from the events in order. Pure: no terminal, no
/// heap, so the rules are tested over a sequence of crossterm events.
#[derive(Debug, Default)]
pub(super) struct KeyTracker {
    /// Each key down, by [`key_identity`], with the event its press delivered.
    down: Vec<(KeyCode, KeyEvent)>,
    /// The held key: the last one pressed, until its release.
    held: Option<KeyCode>,
    /// The keys this terminal has reported a release (or a repeat) of. A press makes its key
    /// held only once it is here, so a key whose release never arrives cannot stay held.
    reported: Vec<KeyCode>,
}

impl KeyTracker {
    pub(super) const fn new() -> Self {
        KeyTracker {
            down: Vec::new(),
            held: None,
            reported: Vec::new(),
        }
    }

    /// The press of the key held now, if one is.
    pub(super) fn held(&self) -> Option<KeyEvent> {
        let held = self.held?;
        self.down
            .iter()
            .find(|(identity, _)| *identity == held)
            .map(|(_, press)| *press)
    }

    /// Fold one key event, `ahead` the input already buffered behind it: what to deliver,
    /// if anything.
    pub(super) fn take<'a>(
        &mut self,
        event: KeyEvent,
        ahead: impl IntoIterator<Item = &'a Event>,
    ) -> Option<KeyDelivery> {
        if !trackable(event.code) {
            return match event.kind {
                KeyEventKind::Release => None,
                _ => Some(KeyDelivery::Press(event)),
            };
        }
        let identity = key_identity(event.code);
        match event.kind {
            KeyEventKind::Press => {
                self.remember(identity, event);
                self.held = self.reported.contains(&identity).then_some(identity);
                Some(KeyDelivery::Press(event))
            }
            KeyEventKind::Repeat => {
                self.learn(identity);
                // a key pressed before the terminal was taken over has no press here
                let press = match self.press_of(identity) {
                    Some(press) => press,
                    None => {
                        self.remember(identity, event);
                        event
                    }
                };
                self.held = Some(identity);
                (!repeat_superseded(identity, ahead)).then_some(KeyDelivery::Press(press))
            }
            KeyEventKind::Release => {
                self.learn(identity);
                match self.down_index(identity, event.code) {
                    Some(at) => {
                        let (released, press) = self.down.remove(at);
                        if self.held == Some(released) {
                            self.held = None;
                        }
                        Some(KeyDelivery::Release(press))
                    }
                    // a key pressed before the terminal was taken over: its own key
                    None => Some(KeyDelivery::Release(event)),
                }
            }
        }
    }

    /// Where the press a release of `identity` names is in `down`. Its own key first; failing
    /// that, a release of a character with Shift already up (`1` for a `!`, `,` for a `<`)
    /// names the latest symbol down, the one its Shift typed. A terminal sends the base key
    /// on such a release and nothing ties it to the glyph without the keyboard layout, so a
    /// symbol is the best guess, and a wrong one only ends a hold early.
    fn down_index(&self, identity: KeyCode, code: KeyCode) -> Option<usize> {
        self.down
            .iter()
            .position(|(down, _)| *down == identity)
            .or_else(|| match code {
                KeyCode::Char(_) => self.down.iter().rposition(
                    |(down, _)| matches!(down, KeyCode::Char(symbol) if !symbol.is_alphanumeric()),
                ),
                _ => None,
            })
    }

    fn press_of(&self, identity: KeyCode) -> Option<KeyEvent> {
        self.down
            .iter()
            .find(|(down, _)| *down == identity)
            .map(|(_, press)| *press)
    }

    fn remember(&mut self, identity: KeyCode, press: KeyEvent) {
        self.down.retain(|(down, _)| *down != identity);
        if self.down.len() >= DOWN_LIMIT {
            let forgotten = self.down.remove(0).0;
            if self.held == Some(forgotten) {
                self.held = None;
            }
        }
        self.down.push((identity, press));
    }

    fn learn(&mut self, identity: KeyCode) {
        if !self.reported.contains(&identity) {
            self.reported.push(identity);
        }
    }
}

/// What one poll yields.
pub(super) enum Polled {
    Key(KeyDelivery),
    Mouse(MouseEvent),
}

/// The terminal's keyboard state: one terminal per OS process, so one of these.
pub(super) struct TerminalKeyboard {
    /// Whether `term-enter` pushed [`FLAGS`], so the leave pops them, once.
    enhanced: bool,
    /// Events read from the tty ahead of the one being delivered.
    ahead: VecDeque<Event>,
    keys: KeyTracker,
}

/// The process's terminal keyboard (`process_wide_statics.rs`: one terminal per process).
static TERMINAL_KEYBOARD: Mutex<TerminalKeyboard> = Mutex::new(TerminalKeyboard {
    enhanced: false,
    ahead: VecDeque::new(),
    keys: KeyTracker::new(),
});

fn keyboard() -> MutexGuard<'static, TerminalKeyboard> {
    TERMINAL_KEYBOARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// After the alternate screen is entered (kitty keeps one flag stack per screen): push
/// [`FLAGS`] when stdout is a terminal that says it speaks the protocol. Best effort; a
/// terminal that does not answer is left as it was.
pub(super) fn enhance() {
    use std::io::IsTerminal;
    let mut keyboard = keyboard();
    keyboard.keys = KeyTracker::new();
    if keyboard.enhanced || !std::io::stdout().is_terminal() {
        return;
    }
    if matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) && crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(FLAGS)
    )
    .is_ok()
    {
        keyboard.enhanced = true;
    }
}

/// Before the alternate screen is left, on every path out: pop what [`enhance`] pushed, and
/// forget the keys. Idempotent; errors are swallowed.
pub(super) fn restore() {
    let mut keyboard = keyboard();
    keyboard.keys = KeyTracker::new();
    if std::mem::take(&mut keyboard.enhanced) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
}

/// The press of the key held now, if one is (never without the protocol).
pub(super) fn held() -> Option<KeyEvent> {
    let keyboard = keyboard();
    if keyboard.enhanced {
        keyboard.keys.held()
    } else {
        None
    }
}

/// Wait up to `timeout` for input and return the next thing to deliver: `None` on timeout,
/// for an event a poll does not report (a resize, focus, paste), and when all the input there
/// was turned out to be superseded repeats. The wait happens without the lock held.
pub(super) fn next(timeout: Duration) -> std::io::Result<Option<Polled>> {
    use crossterm::event::{poll, read};
    if keyboard().ahead.is_empty() && !poll(timeout)? {
        return Ok(None);
    }
    let mut guard = keyboard();
    let TerminalKeyboard {
        enhanced,
        ahead,
        keys,
    } = &mut *guard;
    while ahead.len() < READ_AHEAD && poll(Duration::ZERO)? {
        ahead.push_back(read()?);
    }
    while let Some(event) = ahead.pop_front() {
        match event {
            Event::Key(key) if *enhanced => {
                if let Some(delivery) = keys.take(key, ahead.iter()) {
                    return Ok(Some(Polled::Key(delivery)));
                }
            }
            // without the protocol a release is ignored (some platforms report one anyway)
            // and a repeat is a press, as they always were
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                return Ok(Some(Polled::Key(KeyDelivery::Press(key))));
            }
            Event::Key(_) => {}
            Event::Mouse(mouse) => return Ok(Some(Polled::Mouse(mouse))),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{KeyDelivery, KeyTracker, key_identity, repeat_superseded};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind_and_state(code, modifiers, kind, KeyEventState::NONE)
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        key(code, modifiers, KeyEventKind::Press)
    }

    fn repeat(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        key(code, modifiers, KeyEventKind::Repeat)
    }

    fn release(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        key(code, modifiers, KeyEventKind::Release)
    }

    /// Feed `events` through a tracker the way `next` does — each with the rest of the
    /// sequence as its read-ahead — and collect what is delivered.
    fn deliver(tracker: &mut KeyTracker, events: &[KeyEvent]) -> Vec<KeyDelivery> {
        let buffered: Vec<Event> = events.iter().map(|event| Event::Key(*event)).collect();
        events
            .iter()
            .enumerate()
            .filter_map(|(at, event)| tracker.take(*event, &buffered[at + 1..]))
            .collect()
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;

    // A backlog of repeats behind a slow frame collapses: every repeat followed by another
    // repeat of its key is dropped, and the last one before the release is dropped too, so
    // nothing plays after the key came up. The press and the release are both delivered.
    #[test]
    fn a_buffered_run_of_repeats_is_dropped_up_to_the_release() {
        let n = KeyCode::Char('n');
        let events = [
            press(n, CONTROL),
            repeat(n, CONTROL),
            repeat(n, CONTROL),
            repeat(n, CONTROL),
            release(n, CONTROL),
        ];
        let delivered = deliver(&mut KeyTracker::new(), &events);
        assert_eq!(
            delivered,
            vec![
                KeyDelivery::Press(press(n, CONTROL)),
                KeyDelivery::Release(press(n, CONTROL)),
            ]
        );
    }

    // A repeat with nothing of its key behind it is the live one: it is delivered, as a
    // press of the held key.
    #[test]
    fn the_last_repeat_with_the_key_still_down_is_delivered() {
        let events = [
            press(KeyCode::Down, NONE),
            repeat(KeyCode::Down, NONE),
            repeat(KeyCode::Down, NONE),
        ];
        let delivered = deliver(&mut KeyTracker::new(), &events);
        assert_eq!(
            delivered,
            vec![
                KeyDelivery::Press(press(KeyCode::Down, NONE)),
                KeyDelivery::Press(press(KeyCode::Down, NONE)),
            ]
        );
    }

    // Presses are never dropped, however many of them are buffered: typed text and a
    // terminal that sends repeats as presses read exactly as before.
    #[test]
    fn presses_are_never_dropped() {
        let a = KeyCode::Char('a');
        let events = [press(a, NONE), press(a, NONE), press(a, NONE)];
        let delivered = deliver(&mut KeyTracker::new(), &events);
        assert_eq!(delivered.len(), 3);
        assert!(
            delivered
                .iter()
                .all(|delivery| *delivery == KeyDelivery::Press(press(a, NONE)))
        );
    }

    // Another key's events do not supersede a repeat, and a fresh press of the same key
    // after it does not either: only a later repeat or the release of that key does.
    #[test]
    fn only_a_later_repeat_or_release_of_the_same_key_supersedes() {
        let up = Event::Key(release(KeyCode::Up, NONE));
        let down_repeat = Event::Key(repeat(KeyCode::Down, NONE));
        let down_press = Event::Key(press(KeyCode::Down, NONE));
        assert!(!repeat_superseded(KeyCode::Down, [&up]));
        assert!(repeat_superseded(KeyCode::Down, [&up, &down_repeat]));
        assert!(!repeat_superseded(
            KeyCode::Down,
            [&down_press, &down_repeat]
        ));
        assert!(!repeat_superseded(KeyCode::Down, []));
    }

    // A release names its press by the key, not its modifiers: Ctrl let go before `n` is
    // still `[:key-up :ctrl-n]`, and a shifted letter released after Shift is its press.
    #[test]
    fn a_release_names_its_press_though_the_modifier_came_up_first() {
        let mut tracker = KeyTracker::new();
        let n = KeyCode::Char('n');
        let delivered = deliver(&mut tracker, &[press(n, CONTROL), release(n, NONE)]);
        assert_eq!(delivered[1], KeyDelivery::Release(press(n, CONTROL)));

        let capital = press(KeyCode::Char('A'), KeyModifiers::SHIFT);
        let delivered = deliver(&mut tracker, &[capital, release(KeyCode::Char('a'), NONE)]);
        assert_eq!(delivered[1], KeyDelivery::Release(capital));

        let back_tab = press(KeyCode::BackTab, KeyModifiers::SHIFT);
        let delivered = deliver(&mut tracker, &[back_tab, release(KeyCode::Tab, NONE)]);
        assert_eq!(delivered[1], KeyDelivery::Release(back_tab));
    }

    // A symbol's release with Shift already up arrives as its base key (`,` for `<`), which
    // nothing down matches: it names the latest symbol down.
    #[test]
    fn a_symbols_release_after_shift_names_the_symbol() {
        let angle = press(KeyCode::Char('<'), KeyModifiers::ALT);
        let delivered = deliver(
            &mut KeyTracker::new(),
            &[angle, release(KeyCode::Char(','), NONE)],
        );
        assert_eq!(delivered[1], KeyDelivery::Release(angle));
    }

    // A release with no press seen (the key went down before the terminal was taken over)
    // relays its own key; a bare modifier's release relays nothing.
    #[test]
    fn a_release_with_no_press_relays_its_own_key_and_a_modifier_nothing() {
        let mut tracker = KeyTracker::new();
        let up = release(KeyCode::Up, NONE);
        assert_eq!(tracker.take(up, []), Some(KeyDelivery::Release(up)));
        let shift = release(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift),
            KeyModifiers::SHIFT,
        );
        assert_eq!(tracker.take(shift, []), None);
    }

    // The held key: a repeat makes its key held (the terminal reports its events), the
    // release ends it, and from then on that key is held from its press. A key whose release
    // was never reported is not held from a press, so it cannot stay held.
    #[test]
    fn a_key_is_held_from_its_first_repeat_then_from_its_press() {
        let mut tracker = KeyTracker::new();
        let down = KeyCode::Down;
        tracker.take(press(down, NONE), []);
        assert_eq!(tracker.held(), None, "no release of it reported yet");
        tracker.take(repeat(down, NONE), []);
        assert_eq!(tracker.held(), Some(press(down, NONE)));
        tracker.take(release(down, NONE), []);
        assert_eq!(tracker.held(), None);
        tracker.take(press(down, NONE), []);
        assert_eq!(tracker.held(), Some(press(down, NONE)));

        tracker.take(press(KeyCode::Char('a'), NONE), []);
        assert_eq!(tracker.held(), None, "a's release was never reported");
    }

    // Pressing another key moves the hold to it; releasing the earlier one keeps it there.
    #[test]
    fn the_held_key_is_the_last_pressed() {
        let mut tracker = KeyTracker::new();
        let (left, right) = (KeyCode::Left, KeyCode::Right);
        deliver(
            &mut tracker,
            &[
                press(left, NONE),
                release(left, NONE),
                press(right, NONE),
                release(right, NONE),
            ],
        );
        tracker.take(press(left, NONE), []);
        tracker.take(press(right, NONE), []);
        assert_eq!(tracker.held(), Some(press(right, NONE)));
        tracker.take(release(left, NONE), []);
        assert_eq!(tracker.held(), Some(press(right, NONE)));
        tracker.take(release(right, NONE), []);
        assert_eq!(tracker.held(), None);
    }

    #[test]
    fn a_keys_identity_ignores_case_and_shift_tab() {
        assert_eq!(key_identity(KeyCode::Char('A')), KeyCode::Char('a'));
        assert_eq!(key_identity(KeyCode::BackTab), KeyCode::Tab);
        assert_eq!(key_identity(KeyCode::Up), KeyCode::Up);
    }
}

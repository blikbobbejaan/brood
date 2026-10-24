//! Input translation: winit key and mouse events into the frontend-neutral
//! [`Key`]/[`Mouse`] vocabulary an app `receive`s, plus the click-chain and scroll
//! bookkeeping that turns raw events into `[:key …]` / `[:mouse …]` messages.

use super::*;
use crate::host::gui::{KeyModifiers, named_key};

/// Max gap between consecutive presses (same button, same cell) that still counts
/// as part of one click chain — the double/triple-click window, in milliseconds.
pub(super) const MULTI_CLICK_MS: u128 = 400;

/// A key as the Brood value `term-poll`/`gui` deliver: a printable → a 1-char
/// string, the rest → keywords. Built as a `Message` (no heap) so the GUI thread
/// can deliver it straight to a mailbox (ADR-059). Mirrors `key_to_value`.
pub(super) fn key_message(k: &Key) -> Message {
    match k {
        Key::Char(c) => Message::Str(c.to_string()),
        Key::Ctrl(c) => Message::Keyword(value::intern(&format!("ctrl-{c}"))),
        Key::Alt(c) => Message::Keyword(value::intern(&format!("alt-{c}"))),
        Key::CtrlAlt(c) => Message::Keyword(value::intern(&format!("ctrl-meta-{c}"))),
        Key::Named(s) => Message::Keyword(value::intern(s)),
    }
}

/// A key *release*, as the `[:key-up <key>]` vector — the press value (what
/// `key_message` yields) tagged so the app can tell it from a press. The press
/// itself stays the bare value, so existing dispatch is untouched; release is
/// purely additive. Apps pair down→up to track a held key and drive their own
/// repeat (consumer-paced), rather than relying on the OS auto-repeat we drop.
pub(super) fn key_up_message(k: &Key) -> Message {
    Message::Vector(vec![
        Message::Keyword(value::intern("key-up")),
        key_message(k),
    ])
}

/// A mouse event as the shared `[:mouse action button row col mods]` vector,
/// built as a `Message` (no heap). Mirrors `builtins::mouse_to_value`'s shape so
/// the two frontends stay identical.
pub(super) fn mouse_message(m: &Mouse) -> Message {
    let action = match m.action {
        MouseAction::Press => "press",
        MouseAction::Release => "release",
        MouseAction::Drag => "drag",
        MouseAction::Move => "move",
        MouseAction::ScrollUp => "scroll-up",
        MouseAction::ScrollDown => "scroll-down",
    };
    let button = match m.button {
        Some(MouseButton::Left) => Message::Keyword(value::intern("left")),
        Some(MouseButton::Right) => Message::Keyword(value::intern("right")),
        Some(MouseButton::Middle) => Message::Keyword(value::intern("middle")),
        None => Message::Nil,
    };
    // Held modifiers, in a stable order (ctrl, alt, shift); empty `[]` when none.
    let mut mods = Vec::new();
    if m.ctrl {
        mods.push(Message::Keyword(value::intern("ctrl")));
    }
    if m.alt {
        mods.push(Message::Keyword(value::intern("alt")));
    }
    if m.shift {
        mods.push(Message::Keyword(value::intern("shift")));
    }
    let mut v = vec![
        Message::Keyword(value::intern("mouse")),
        Message::Keyword(value::intern(action)),
        button,
        Message::Int(m.row as i64),
        Message::Int(m.col as i64),
        Message::Vector(mods),
    ];
    // A press carries its click-chain count as a trailing 7th element.
    // A scroll carries the line-unit delta as a trailing 7th element (float).
    // Mirrors `builtins::mouse_value`.
    if m.count > 0 {
        v.push(Message::Int(m.count as i64));
    } else if m.scroll_dy != 0.0 {
        v.push(Message::Float(m.scroll_dy));
    }
    // The pointer's exact position, as a trailing `{:at [row col]}` in FRACTIONAL window
    // cells — after the 7th slot, which is filled with nil when there is no count or delta
    // so every consumer reading those slots by position reads what it always did.
    if let Some((row, col)) = m.at {
        if v.len() == 6 {
            v.push(Message::Nil);
        }
        v.push(Message::Map(vec![(
            Message::Keyword(value::intern("at")),
            Message::Vector(vec![Message::Float(row), Message::Float(col)]),
        )]));
    }
    Message::Vector(v)
}

/// A synthetic release of held button `b` at cell `(col, row)` — delivered when the
/// pointer leaves the window or focus is lost while a button is down, so its real
/// (off-window) release can't strand the app thinking the button is still pressed.
pub(super) fn release_of(
    b: MouseButton,
    col: u16,
    row: u16,
    at: Option<(f64, f64)>,
    mods: &ModifiersState,
) -> Mouse {
    Mouse {
        action: MouseAction::Release,
        button: Some(b),
        row,
        col,
        ctrl: mods.control_key(),
        alt: mods.alt_key(),
        shift: mods.shift_key(),
        count: 0,
        scroll_dy: 0.0,
        at,
    }
}

/// A resize event as the `[:resize cols rows]` vector (the new cell grid),
/// built as a `Message` (no heap) so the GUI thread can deliver it to a
/// mailbox. Wakes the app loop so it re-renders at the new size instead of
/// waiting out its poll timeout.
pub(super) fn resize_message(cols: u16, rows: u16) -> Message {
    Message::Vector(vec![
        Message::Keyword(value::intern("resize")),
        Message::Int(cols as i64),
        Message::Int(rows as i64),
    ])
}

/// The `[:resize w h]` a PIXEL-input window gets: the same shape as `resize_message`,
/// in physical pixels — the unit every other message of that window uses.
pub(super) fn resize_message_px(w: u32, h: u32) -> Message {
    Message::Vector(vec![
        Message::Keyword(value::intern("resize")),
        Message::Int(w as i64),
        Message::Int(h as i64),
    ])
}

/// Deliver a scroll event to a window's subscriber. `dy > 0` is scroll-up, `dy < 0` is
/// scroll-down. Modifiers come from the window's current modifier state. Used both for
/// live gesture events and the kinetic-momentum synthetic events fired after gesture end.
pub(super) fn deliver_scroll(w: &Win, dy: f64) {
    let action = if dy > 0.0 {
        MouseAction::ScrollUp
    } else {
        MouseAction::ScrollDown
    };
    let (col, row) = w.cursor;
    deliver(
        w.subscriber,
        mouse_message(&Mouse {
            action,
            button: None,
            row,
            col,
            ctrl: w.mods.control_key(),
            alt: w.mods.alt_key(),
            shift: w.mods.shift_key(),
            count: 0,
            scroll_dy: dy.abs(),
            at: w.pointer_at(),
        }),
    );
}

pub(super) fn translate_button(b: WMouseButton) -> Option<MouseButton> {
    match b {
        WMouseButton::Left => Some(MouseButton::Left),
        WMouseButton::Right => Some(MouseButton::Right),
        WMouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

/// The US-layout shifted form of a base character — `.` → `>`, `1` → `!`, etc.
/// Used to re-apply Shift to a modifier chord whose base came from
/// `key_without_modifiers()` (which drops Shift along with Ctrl/Alt). Letters and
/// anything without a shifted punctuation form pass through unchanged (a letter's
/// shift is just upper-case, and the chord is lower-cased anyway). Matches the glyphs
/// the crossterm frontend reports for the same physical chord.
pub(super) fn shift_char(c: char) -> char {
    match c {
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        other => other,
    }
}

/// winit's modifier state as the frontend-neutral set a key name spells.
pub(super) fn key_modifiers(mods: ModifiersState) -> KeyModifiers {
    KeyModifiers {
        ctrl: mods.control_key(),
        alt: mods.alt_key(),
        shift: mods.shift_key(),
        super_key: mods.super_key(),
    }
}

/// The base name of a named key that carries its modifiers in the name — the arrows,
/// editing keys, F1–F24 and the rest of the keys a keyboard has beyond the typewriter
/// ones (ADR-441). Tab, Escape and Space are not here: each has its own rule in
/// `translate_key`. The names are the terminal frontend's (`builtins::key_to_value`)
/// and `std/vt`'s, so a key reads the same in a window, in a terminal and to a program
/// under a pty. Lock keys, PrintScreen and bare modifiers are not keys an app binds.
fn named_key_base(key: &NamedKey) -> Option<&'static str> {
    Some(match key {
        NamedKey::ArrowUp => "up",
        NamedKey::ArrowDown => "down",
        NamedKey::ArrowLeft => "left",
        NamedKey::ArrowRight => "right",
        NamedKey::Enter => "enter",
        NamedKey::Backspace => "backspace",
        NamedKey::Delete => "delete",
        NamedKey::Insert => "insert",
        NamedKey::Home => "home",
        NamedKey::End => "end",
        NamedKey::PageUp => "page-up",
        NamedKey::PageDown => "page-down",
        NamedKey::ContextMenu => "menu",
        NamedKey::Pause => "pause",
        NamedKey::ScrollLock => "scroll-lock",
        NamedKey::Help => "help",
        NamedKey::Find => "find",
        NamedKey::Undo => "undo",
        NamedKey::Redo => "redo",
        NamedKey::Copy => "copy",
        NamedKey::Cut => "cut",
        NamedKey::Paste => "paste",
        NamedKey::Select => "select",
        NamedKey::Execute => "execute",
        NamedKey::Cancel => "cancel",
        NamedKey::Clear => "clear",
        NamedKey::BrowserBack => "back",
        NamedKey::BrowserForward => "forward",
        NamedKey::AudioVolumeUp => "volume-up",
        NamedKey::AudioVolumeDown => "volume-down",
        NamedKey::AudioVolumeMute => "volume-mute",
        NamedKey::MediaPlayPause => "media-play-pause",
        NamedKey::MediaStop => "media-stop",
        NamedKey::MediaTrackNext => "media-next",
        NamedKey::MediaTrackPrevious => "media-previous",
        NamedKey::F1 => "f1",
        NamedKey::F2 => "f2",
        NamedKey::F3 => "f3",
        NamedKey::F4 => "f4",
        NamedKey::F5 => "f5",
        NamedKey::F6 => "f6",
        NamedKey::F7 => "f7",
        NamedKey::F8 => "f8",
        NamedKey::F9 => "f9",
        NamedKey::F10 => "f10",
        NamedKey::F11 => "f11",
        NamedKey::F12 => "f12",
        NamedKey::F13 => "f13",
        NamedKey::F14 => "f14",
        NamedKey::F15 => "f15",
        NamedKey::F16 => "f16",
        NamedKey::F17 => "f17",
        NamedKey::F18 => "f18",
        NamedKey::F19 => "f19",
        NamedKey::F20 => "f20",
        NamedKey::F21 => "f21",
        NamedKey::F22 => "f22",
        NamedKey::F23 => "f23",
        NamedKey::F24 => "f24",
        _ => return None,
    })
}

/// The glyph a chord with Shift held names (Emacs `M->` is Alt+Shift+`.`). The layout
/// decides it: winit's `logical_key` has Shift (and the layout's levels) applied but not
/// Control, so on a German layout Alt+Shift+`.` is `M-:` and Alt+Shift+`<` is `M->`, as
/// the keycaps say. Two cases fall back: a logical key that is not plain printable ASCII
/// — macOS applies Option to it too (Option+Shift+`.` is `˘`), which is why chords read
/// `key_without_modifiers` in the first place — goes through the US table on the base key
/// (`shift_char`), and a letter keeps its base (the case is decided by the caller).
pub(super) fn shifted_chord_char(logical: Option<char>, base: char) -> char {
    match logical {
        Some(glyph) if glyph.is_ascii_graphic() && glyph != base => glyph,
        _ if base.is_alphabetic() => base,
        _ => shift_char(base),
    }
}

/// The text a plain keystroke (no Control / Alt / Super) typed, when it is MORE than one
/// character — a compose sequence or a layout key that types several. Delivered as
/// `[:text "…"]` rather than squeezed into one key (the first character was all that
/// survived before ADR-441). A one-character keystroke is not text: it stays the 1-char
/// key string `translate_key` makes of it, so every keymap that binds a character binds
/// it however it was typed.
pub(super) fn typed_text(ke: &KeyEvent, mods: ModifiersState) -> Option<String> {
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return None;
    }
    let WKey::Character(_) = &ke.logical_key else {
        return None;
    };
    let text = ke.text.as_deref()?;
    let mut chars = text.chars();
    chars.next()?;
    (chars.next().is_some() && !text.chars().any(char::is_control)).then(|| text.to_string())
}

pub(super) fn translate_key(ke: &KeyEvent, mods: ModifiersState) -> Option<Key> {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    let modifiers = key_modifiers(mods);
    match &ke.logical_key {
        // Space carries modifiers like a character key would, so Ctrl/Alt survive (Emacs
        // `C-SPC` set-mark → :ctrl- , `C-M-SPC` mark-sexp → :ctrl-meta- , matching
        // crossterm) rather than collapsing to a self-inserted space.
        WKey::Named(NamedKey::Space) => Some(character_chord(' ', modifiers)),
        // Shift+Tab is back-tab — match the crossterm frontend's :back-tab.
        WKey::Named(NamedKey::Tab) if modifiers.shift => Some(Key::Named("back-tab")),
        WKey::Named(NamedKey::Tab) => Some(Key::Named("tab")),
        WKey::Named(NamedKey::Escape) => Some(Key::Named("escape")),
        // The named keys carry their modifiers in the name (`crate::host::gui::named_key`,
        // the rule both frontends share): `:ctrl-left`, `:alt-shift-up`, `:super-f5`, … —
        // so the editor binds `C-<left>` / `C-S-<arrow>` as distinctly as `:shift-up`
        // (shift-select) from a plain arrow.
        WKey::Named(named) => Some(Key::Named(named_key(named_key_base(named)?, modifiers))),
        WKey::Character(s) => {
            let chord = modifiers.ctrl || modifiers.alt || modifiers.super_key;
            if !chord {
                // Plain typing keeps what the key TYPED: `ke.text` has dead keys and
                // compose applied (`´` then `e` types `é`), where the logical key is the
                // bare `e`. A release has no text, so it falls back to the logical key —
                // a release is matched to its press by the physical key, never by this.
                let typed = ke.text.as_deref().and_then(|t| t.chars().next());
                let c = typed
                    .filter(|c| !c.is_control())
                    .or_else(|| s.chars().next())?;
                return Some(Key::Char(c));
            }
            // For a chord, read the key WITHOUT modifiers, so layout composition (on
            // some layouts Alt+`-` composes to en-dash `–`, macOS Option+letters to
            // accents) doesn't mangle the chord — the keymap binds the BASE character
            // (`-`, `f`). `key_without_modifiers()` also strips SHIFT, so a chord with
            // Shift held takes its glyph from the layout (`shifted_chord_char`): Emacs
            // `M->` = Alt+Shift+`.` must reach the `alt->` binding, not `alt-.`.
            let logical = s.chars().next();
            let base = match ke.key_without_modifiers() {
                WKey::Character(b) => b.chars().next(),
                _ => logical,
            }?;
            let c = if modifiers.shift {
                shifted_chord_char(logical, base)
            } else {
                base
            };
            Some(character_chord(c, modifiers))
        }
        _ => None,
    }
}

/// A character `c` under chord `modifiers`, Shift already folded into `c`. Control chords
/// are case-INsensitive (Emacs `C-a` = `C-A`); Meta and Super are case-SENSITIVE (`M-O`
/// open-line-above ≠ `M-o`): a shifted letter stays upper-case so the two are distinct,
/// while an unshifted chord lower-cases (so Caps Lock / a stray Shift can't change the
/// binding). A chord with Super spells through `named_key` (`:super-x`, `:ctrl-super-x`).
fn character_chord(c: char, modifiers: KeyModifiers) -> Key {
    let case_kept = if !modifiers.ctrl && modifiers.shift {
        c.to_ascii_uppercase()
    } else {
        c.to_ascii_lowercase()
    };
    match modifiers {
        KeyModifiers {
            super_key: true, ..
        } => Key::Named(named_key(
            &case_kept.to_string(),
            KeyModifiers {
                shift: false,
                ..modifiers
            },
        )),
        KeyModifiers {
            ctrl: true,
            alt: true,
            ..
        } => Key::CtrlAlt(c.to_ascii_lowercase()),
        KeyModifiers { ctrl: true, .. } => Key::Ctrl(c.to_ascii_lowercase()),
        KeyModifiers { alt: true, .. } => Key::Alt(case_kept),
        _ => Key::Char(c),
    }
}

// ---- input methods and focus ------------------------------------------------------------

/// A commit of composed text as `[:text "…"]` — what an input method (or a keystroke
/// that typed several characters) inserts at point.
pub(super) fn text_message(text: &str) -> Message {
    Message::Vector(vec![
        Message::Keyword(value::intern("text")),
        Message::Str(text.to_string()),
    ])
}

/// The input method's composition in progress, as `[:preedit "…" [from to]]`: the text
/// to show at point (not yet inserted), and the IME's cursor inside it as a CHARACTER
/// range — winit reports bytes, which a Brood string does not index by — or nil when the
/// IME hides it. An empty text with nil clears the composition.
pub(super) fn preedit_message(text: &str, cursor: Option<(usize, usize)>) -> Message {
    let char_index = |byte: usize| text[..byte.min(text.len())].chars().count() as i64;
    let range = match cursor {
        Some((from, to)) if text.is_char_boundary(from) && text.is_char_boundary(to) => {
            Message::Vector(vec![
                Message::Int(char_index(from)),
                Message::Int(char_index(to)),
            ])
        }
        _ => Message::Nil,
    };
    Message::Vector(vec![
        Message::Keyword(value::intern("preedit")),
        Message::Str(text.to_string()),
        range,
    ])
}

/// What an IME commit becomes. A single character typed with NO composition shown is a
/// keystroke an input method passed through (some IMEs commit every key they do not
/// compose), so it is the 1-char key string a keyboard would have sent — the keymap
/// cannot tell the difference, and must not. Anything that ended a composition, or is
/// longer than one character, is `[:text "…"]`.
pub(super) fn commit_message(text: &str, after_preedit: bool) -> Option<Message> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if !after_preedit && chars.next().is_none() && !first.is_control() {
        Some(key_message(&Key::Char(first)))
    } else {
        Some(text_message(text))
    }
}

/// Where the IME's candidate window goes: the first `Op::Cursor` of the frame, as a
/// grid-relative PIXEL rect `(x, y, w, h)` one cell in size, walking INTO the regions
/// with the coordinate math `render_ops` paints them with (a cursor in a scrolled or
/// zoomed pane is placed where it is drawn). `None` when the frame has no cursor.
pub(super) fn ime_cursor_area(ops: &[Op], r: &Renderer) -> Option<(isize, isize, usize, usize)> {
    find_cursor(ops, r, 0, 0, r.cell_w.max(1), r.cell_h.max(1), 0)
}

fn find_cursor(
    ops: &[Op],
    r: &Renderer,
    ox: isize,
    oy: isize,
    cw: usize,
    ch: usize,
    scroll_dy: isize,
) -> Option<(isize, isize, usize, usize)> {
    ops.iter().find_map(|op| match op {
        Op::Cursor { row, col, .. } => Some((
            ox.saturating_add((*col as isize).saturating_mul(cw as isize)),
            oy.saturating_add((*row as isize).saturating_mul(ch as isize))
                .saturating_sub(scroll_dy),
            cw,
            ch,
        )),
        Op::ScrollRegion { dy_frac, ops } => {
            find_cursor(ops, r, ox, oy, cw, ch, scroll_px(*dy_frac, ch))
        }
        Op::CellRegion { x, y, px, ops, .. } => {
            let region = r.metrics_at(*px);
            if region.cell_w == 0 || region.cell_h == 0 {
                return None;
            }
            find_cursor(
                ops,
                r,
                ox.saturating_add((*x as isize).saturating_mul(cw as isize)),
                oy.saturating_add((*y as isize).saturating_mul(ch as isize))
                    .saturating_sub(scroll_dy),
                region.cell_w,
                region.cell_h,
                0,
            )
        }
        _ => None,
    })
}

/// The gate that keeps at most ONE OS auto-repeat undelivered per window
/// (`{:key-repeat :os}`, ADR-441). A repeat is delivered with a credit charged here; the
/// credit comes back when the message leaves the mailbox (`process::deliver_credited`),
/// and until it does, further repeats are dropped. So an app whose frame is slower than
/// the repeat rate repeats once per turn instead of queueing a burst that plays on after
/// the key is released — the ADR-086 bug — while a fast app gets every repeat.
#[derive(Default)]
pub(super) struct RepeatGate {
    outstanding: AtomicBool,
}

impl RepeatGate {
    /// A credit for one repeat, or `None` while the last one is still queued.
    pub(super) fn charge(gate: &Arc<RepeatGate>) -> Option<crate::process::DeliveryCredit> {
        gate.outstanding
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(crate::process::DeliveryCredit::new(gate.clone(), 1))
    }
}

impl crate::process::CreditSource for RepeatGate {
    fn release(&self, _cost: usize) {
        self.outstanding.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod shift_char_tests {
    use super::shift_char;
    #[test]
    fn maps_us_shifted_punctuation() {
        assert_eq!(shift_char('.'), '>'); // Emacs M-> (end-of-buffer)
        assert_eq!(shift_char(','), '<'); // M-< (beginning-of-buffer)
        assert_eq!(shift_char('['), '{');
        assert_eq!(shift_char(']'), '}'); // M-{ / M-} paragraph motion
        assert_eq!(shift_char('5'), '%'); // M-% (query-replace)
        assert_eq!(shift_char('6'), '^'); // M-^ (join-line)
        assert_eq!(shift_char('f'), 'f'); // letters pass through (lower-cased later)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press_at(at: Option<(f64, f64)>, count: u8) -> Vec<Message> {
        let m = Mouse {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            row: 3,
            col: 7,
            ctrl: false,
            alt: false,
            shift: false,
            count,
            scroll_dy: 0.0,
            at,
        };
        // `Message` implements `Drop` (its children are freed iteratively), so the vector
        // is taken out by `mem::take` rather than moved out of the pattern.
        match &mut mouse_message(&m) {
            Message::Vector(v) => std::mem::take(v),
            _ => panic!("a mouse message is a vector"),
        }
    }

    #[test]
    fn the_exact_position_trails_the_positional_slots() {
        // a press: slot 6 stays the click count, the position follows it
        let v = press_at(Some((3.75, 7.25)), 1);
        assert_eq!(v.len(), 8);
        assert!(matches!(v[3], Message::Int(3)));
        assert!(matches!(v[6], Message::Int(1)));
        let Message::Map(entries) = &v[7] else {
            panic!("the position is a map")
        };
        let (_, Message::Vector(at)) = &entries[0] else {
            panic!("{{:at [row col]}}")
        };
        assert!(matches!(at[0], Message::Float(r) if r == 3.75));
        assert!(matches!(at[1], Message::Float(c) if c == 7.25));
    }

    #[test]
    fn with_nothing_in_slot_six_it_is_nil_so_the_slots_keep_their_places() {
        let v = press_at(Some((1.5, 2.5)), 0);
        assert_eq!(v.len(), 8);
        assert!(matches!(v[6], Message::Nil));
    }

    #[test]
    fn no_position_is_the_message_it_always_was() {
        let v = press_at(None, 2);
        assert_eq!(v.len(), 7);
    }
}

#[cfg(test)]
mod adr_431_tests {
    use super::*;

    fn modifiers(ctrl: bool, alt: bool, shift: bool, super_key: bool) -> KeyModifiers {
        KeyModifiers {
            ctrl,
            alt,
            shift,
            super_key,
        }
    }

    fn keyword_of(key: Key) -> String {
        match key {
            Key::Named(name) => name.to_string(),
            Key::Ctrl(c) => format!("ctrl-{c}"),
            Key::Alt(c) => format!("alt-{c}"),
            Key::CtrlAlt(c) => format!("ctrl-meta-{c}"),
            Key::Char(c) => c.to_string(),
        }
    }

    #[test]
    fn a_shifted_chord_takes_the_layouts_glyph() {
        // US: Alt+Shift+. is M->
        assert_eq!(shifted_chord_char(Some('>'), '.'), '>');
        // German: Shift+. is `:` and Shift+7 is `/` — the keycaps, not the US table
        assert_eq!(shifted_chord_char(Some(':'), '.'), ':');
        assert_eq!(shifted_chord_char(Some('/'), '7'), '/');
        // macOS applies Option to the logical key: the US table answers instead
        assert_eq!(shifted_chord_char(Some('˘'), '.'), '>');
        // a letter keeps its base; the case is the chord's business
        assert_eq!(shifted_chord_char(Some('Å'), 'a'), 'a');
        assert_eq!(shifted_chord_char(Some('A'), 'a'), 'A');
    }

    #[test]
    fn super_chords_spell_emacs_order() {
        assert_eq!(
            keyword_of(character_chord('x', modifiers(false, false, false, true))),
            "super-x"
        );
        assert_eq!(
            keyword_of(character_chord('S', modifiers(false, false, true, true))),
            "super-S"
        );
        assert_eq!(
            keyword_of(character_chord('S', modifiers(true, false, true, true))),
            "ctrl-super-s"
        );
        assert_eq!(
            keyword_of(character_chord('>', modifiers(false, true, true, true))),
            "alt-super->"
        );
    }

    #[test]
    fn chords_without_super_are_what_they_always_were() {
        assert_eq!(
            keyword_of(character_chord('f', modifiers(true, true, false, false))),
            "ctrl-meta-f"
        );
        assert_eq!(
            keyword_of(character_chord('O', modifiers(false, true, true, false))),
            "alt-O"
        );
        assert_eq!(
            keyword_of(character_chord('A', modifiers(true, false, true, false))),
            "ctrl-a"
        );
        assert_eq!(
            keyword_of(character_chord(' ', modifiers(true, false, false, false))),
            "ctrl- "
        );
        assert_eq!(
            keyword_of(character_chord(' ', modifiers(false, false, false, false))),
            " "
        );
    }

    #[test]
    fn named_keys_cover_the_function_and_editing_keys() {
        assert_eq!(named_key_base(&NamedKey::F1), Some("f1"));
        assert_eq!(named_key_base(&NamedKey::F24), Some("f24"));
        assert_eq!(named_key_base(&NamedKey::Insert), Some("insert"));
        assert_eq!(named_key_base(&NamedKey::ContextMenu), Some("menu"));
        assert_eq!(named_key_base(&NamedKey::PrintScreen), None);
        assert_eq!(named_key_base(&NamedKey::Shift), None);
    }

    fn vector_of(message: Message) -> Vec<Message> {
        match &mut { message } {
            Message::Vector(items) => std::mem::take(items),
            _ => panic!("a vector message"),
        }
    }

    #[test]
    fn a_preedit_cursor_is_counted_in_characters() {
        // "啊b" — the IME's cursor after the first character is byte 3
        let items = vector_of(preedit_message("啊b", Some((3, 3))));
        assert!(matches!(&items[1], Message::Str(s) if s == "啊b"));
        let Message::Vector(range) = &items[2] else {
            panic!("a range")
        };
        assert!(matches!(range[0], Message::Int(1)));
        assert!(matches!(range[1], Message::Int(1)));
        // the clearing preedit
        let cleared = vector_of(preedit_message("", None));
        assert!(matches!(cleared[2], Message::Nil));
    }

    #[test]
    fn a_passed_through_character_is_a_key_and_a_composition_is_text() {
        assert!(matches!(&commit_message("a", false), Some(Message::Str(s)) if s == "a"));
        let composed = vector_of(commit_message("é", true).expect("a commit"));
        assert!(matches!(&composed[1], Message::Str(s) if s == "é"));
        let several = vector_of(commit_message("啊不", false).expect("a commit"));
        assert!(matches!(&several[1], Message::Str(s) if s == "啊不"));
        assert!(commit_message("", true).is_none());
    }

    #[test]
    fn the_repeat_gate_holds_one_until_it_is_received() {
        let gate = Arc::new(RepeatGate::default());
        let credit = RepeatGate::charge(&gate).expect("the first repeat goes");
        assert!(
            RepeatGate::charge(&gate).is_none(),
            "a second waits while the first is queued"
        );
        drop(credit); // the message left the mailbox
        assert!(RepeatGate::charge(&gate).is_some());
    }
}

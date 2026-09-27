//! Turns the phone's touchpad screen into input for a remote computer.
//!
//! The Slint page reports finger motion, taps and typed text; this module
//! converts them to the protocol's pointer and key events and hands them to
//! the service through the touchpad capture backend, so they travel the
//! same authenticated path as input captured on a desktop.

use syntra_input_capture::{Position, touchpad};
use syntra_input_event::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent};

const KEY_LEFTSHIFT: u32 = 42;
const KEY_BACKSPACE: u32 = 14;
const KEY_ENTER: u32 = 28;

/// Maps the UI's zone index (the peer map's order) to a capture position.
pub fn position(zone: i32) -> Position {
    match zone {
        1 => Position::Right,
        2 => Position::Top,
        3 => Position::Bottom,
        _ => Position::Left,
    }
}

/// Starts controlling the device placed at `zone`.
pub fn begin(zone: i32) {
    touchpad::sender().begin(position(zone));
}

/// Moves the remote pointer by a finger displacement, in logical pixels.
pub fn motion(zone: i32, dx: f32, dy: f32, sensitivity: f32) {
    touchpad::sender().input(
        position(zone),
        Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: f64::from(dx * sensitivity),
            dy: f64::from(dy * sensitivity),
        }),
    );
}

/// Presses or releases a mouse button: 0 left, 1 right, 2 middle.
pub fn button(zone: i32, which: i32, pressed: bool) {
    let button = match which {
        1 => BTN_RIGHT,
        2 => BTN_MIDDLE,
        _ => BTN_LEFT,
    };
    touchpad::sender().input(
        position(zone),
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button,
            state: u32::from(pressed),
        }),
    );
}

/// Scrolls vertically by a finger displacement; down is positive.
pub fn scroll(zone: i32, dy: f32) {
    // One detent is 120; a 40 px drag should feel like one wheel click.
    let value = (dy * 3.0).round() as i32;
    if value == 0 {
        return;
    }
    touchpad::sender().input(
        position(zone),
        Event::Pointer(PointerEvent::AxisDiscrete120 { axis: 0, value }),
    );
}

/// Types text on the remote machine as US-layout key strokes.
pub fn type_text(zone: i32, text: &str) {
    let sender = touchpad::sender();
    let position = position(zone);
    let key = |code: u32, pressed: bool| {
        sender.input(
            position,
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: code,
                state: u8::from(pressed),
            }),
        );
    };
    for ch in text.chars() {
        let Some((code, shift)) = us_key(ch) else {
            log::debug!("no key for {ch:?}");
            continue;
        };
        if shift {
            key(KEY_LEFTSHIFT, true);
        }
        key(code, true);
        key(code, false);
        if shift {
            key(KEY_LEFTSHIFT, false);
        }
    }
}

/// Deletes the character before the remote cursor.
pub fn backspace(zone: i32) {
    tap_key(zone, KEY_BACKSPACE);
}

/// Presses Enter on the remote machine.
pub fn enter(zone: i32) {
    tap_key(zone, KEY_ENTER);
}

fn tap_key(zone: i32, code: u32) {
    let sender = touchpad::sender();
    for pressed in [true, false] {
        sender.input(
            position(zone),
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: code,
                state: u8::from(pressed),
            }),
        );
    }
}

/// Linux key code and whether Shift is needed, for a US keyboard layout.
fn us_key(ch: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const DIGITS: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    Some(match ch {
        'a'..='z' => (LETTERS[(ch as u8 - b'a') as usize], false),
        'A'..='Z' => (LETTERS[(ch as u8 - b'A') as usize], true),
        '0'..='9' => (DIGITS[(ch as u8 - b'0') as usize], false),
        ' ' => (57, false),
        '\n' => (KEY_ENTER, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::us_key;

    #[test]
    fn us_layout_covers_letters_digits_and_shifted_symbols() {
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('Z'), Some((44, true)));
        assert_eq!(us_key('0'), Some((11, false)));
        assert_eq!(us_key('@'), Some((3, true)));
        assert_eq!(us_key('é'), None);
    }
}

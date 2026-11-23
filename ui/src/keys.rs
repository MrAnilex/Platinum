//! A key pressed in the shortcuts panel, turned into the combination Windows registers.
//!
//! Slint gives a key as the text it types, plus the modifiers held. That text depends on the
//! keyboard layout and on the modifiers (on a French keyboard the "1" key types "&"), and the
//! numeric keypad types the same digits as the top row: Windows' virtual-key code, which
//! `RegisterHotKey` wants, cannot always be told from it. So the key that is down is asked from
//! Windows first (`GetAsyncKeyState`, while the window has the keyboard), and the text is only the
//! fallback, for a key already released.

use platinum::hotkeys::{self, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use platinum::i18n::tr;
use platinum::trf;
use slint::platform::Key;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetAsyncKeyState(key: i32) -> i16;
}

/// Slint's special keys and their virtual-key codes.
const SPECIAL: [(Key, u32); 41] = [
    (Key::Backspace, 0x08),
    (Key::Tab, 0x09),
    (Key::Return, 0x0D),
    (Key::Space, 0x20),
    (Key::Delete, 0x2E),
    (Key::Insert, 0x2D),
    (Key::Home, 0x24),
    (Key::End, 0x23),
    (Key::PageUp, 0x21),
    (Key::PageDown, 0x22),
    (Key::LeftArrow, 0x25),
    (Key::UpArrow, 0x26),
    (Key::RightArrow, 0x27),
    (Key::DownArrow, 0x28),
    (Key::Pause, 0x13),
    (Key::ScrollLock, 0x91),
    (Key::SysReq, 0x2C),
    (Key::F1, 0x70),
    (Key::F2, 0x71),
    (Key::F3, 0x72),
    (Key::F4, 0x73),
    (Key::F5, 0x74),
    (Key::F6, 0x75),
    (Key::F7, 0x76),
    (Key::F8, 0x77),
    (Key::F9, 0x78),
    (Key::F10, 0x79),
    (Key::F11, 0x7A),
    (Key::F12, 0x7B),
    (Key::F13, 0x7C),
    (Key::F14, 0x7D),
    (Key::F15, 0x7E),
    (Key::F16, 0x7F),
    (Key::F17, 0x80),
    (Key::F18, 0x81),
    (Key::F19, 0x82),
    (Key::F20, 0x83),
    (Key::F21, 0x84),
    (Key::F22, 0x85),
    (Key::F23, 0x86),
    (Key::F24, 0x87),
];

/// The modifier keys: pressed alone, they start a combination and the field keeps waiting.
const MODIFIERS: [Key; 9] = [Key::Shift, Key::ShiftR, Key::Control, Key::ControlR, Key::Alt, Key::AltGr, Key::Meta, Key::MetaR, Key::CapsLock];

/// One character of text as Slint gives a key.
fn single(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Whether the key pressed is a modifier alone.
pub fn is_modifier(text: &str) -> bool {
    single(text).is_some_and(|c| MODIFIERS.iter().any(|key| char::from(*key) == c))
}

/// The virtual-key code a key's text stands for: a special key, a letter or a digit.
pub fn from_text(text: &str) -> Option<u32> {
    let c = single(text)?;
    if let Some((_, code)) = SPECIAL.iter().find(|(key, _)| char::from(*key) == c) {
        return Some(*code);
    }
    c.is_ascii_alphanumeric().then(|| c.to_ascii_uppercase() as u32)
}

/// Whether a virtual-key code is a modifier (Shift, Ctrl, Alt, Windows, Caps Lock).
fn modifier_code(code: u32) -> bool {
    matches!(code, 0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// The keys a shortcut may use that Windows says are down now.
fn keys_down() -> Vec<u32> {
    (0x08u32..=0xFE).filter(|&code| !modifier_code(code) && hotkeys::key_name(code).is_some()).filter(|&code| unsafe { GetAsyncKeyState(code as i32) } as u16 & 0x8000 != 0).collect()
}

/// The key of a press: the one down (the text's when several are), else the text's.
fn pick(down: &[u32], typed: Option<u32>) -> Option<u32> {
    match (down, typed) {
        ([one], _) => Some(*one),
        (many, Some(typed)) if many.contains(&typed) => Some(typed),
        ([first, ..], None) => Some(*first),
        (_, typed) => typed,
    }
}

/// What a key pressed while a field waits makes: `None` for a modifier alone (the field keeps
/// waiting), else the combination, or why it cannot be one (in French).
pub fn combination(text: &str, control: bool, alt: bool, shift: bool, meta: bool) -> Option<Result<Hotkey, String>> {
    if is_modifier(text) {
        return None;
    }
    let Some(key) = pick(&keys_down(), from_text(text)) else {
        return Some(Err(tr("Cette touche ne peut pas servir de raccourci.").into()));
    };
    let flags = [(control, MOD_CONTROL), (alt, MOD_ALT), (shift, MOD_SHIFT), (meta, MOD_WIN)].into_iter().filter(|(held, _)| *held).fold(0, |all, (_, flag)| all | flag);
    let hotkey = Hotkey::new(flags, key);
    Some(hotkey.check().map(|()| hotkey).map_err(|problem| trf!("{keys} : {problem}.", keys = hotkey.display(), problem = problem)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slints_keys_become_windows_codes() {
        assert_eq!(from_text(&slint::SharedString::from(Key::UpArrow)), Some(0x26));
        assert_eq!(from_text(&slint::SharedString::from(Key::F13)), Some(0x7C));
        assert_eq!(from_text(&slint::SharedString::from(Key::Delete)), Some(0x2E));
        assert_eq!(from_text("m"), Some(0x4D));
        assert_eq!(from_text("7"), Some(0x37));
        // A French keyboard's "1" key types "&": only Windows can tell which key it was.
        assert_eq!(from_text("&"), None);
        assert_eq!(from_text("ab"), None);
        assert!(is_modifier(&slint::SharedString::from(Key::Control)) && is_modifier(&slint::SharedString::from(Key::AltGr)));
        assert!(!is_modifier("a") && !is_modifier(""));
        // Every special key has a name in platinum.ini.
        assert!(SPECIAL.iter().all(|(_, code)| hotkeys::key_name(*code).is_some()));
    }

    #[test]
    fn the_key_down_wins_over_the_text() {
        // The keypad's 8 types "8", like the top row: Windows says which one is down.
        assert_eq!(pick(&[0x68], Some(0x38)), Some(0x68));
        assert_eq!(pick(&[0x41, 0x26], Some(0x26)), Some(0x26));
        assert_eq!(pick(&[0x41, 0x26], None), Some(0x41));
        assert_eq!(pick(&[], Some(0x26)), Some(0x26));
        assert_eq!(pick(&[], None), None);
    }
}

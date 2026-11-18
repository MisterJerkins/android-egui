use egui::{Event, Key, Modifiers};

use super::{Language, REPEAT_DELAY, REPEAT_INTERVAL};

const LEFT_CTRL: u16 = 29;
const LEFT_SHIFT: u16 = 42;
const RIGHT_SHIFT: u16 = 54;
const LEFT_ALT: u16 = 56;
const SPACE: u16 = 57;
const CAPS_LOCK: u16 = 58;
const RIGHT_CTRL: u16 = 97;
const RIGHT_ALT: u16 = 100;
const LEFT_META: u16 = 125;
const RIGHT_META: u16 = 126;
const MODIFIER_KEYS: [u16; 8] = [LEFT_SHIFT, RIGHT_SHIFT, LEFT_ALT, RIGHT_ALT, LEFT_CTRL, RIGHT_CTRL, LEFT_META, RIGHT_META];

/// Characters by key code, plain and shifted, for each language.
const LAYOUTS: [[&str; 2]; 2] = [
    ["\0\x001234567890-=\0\0qwertyuiop[]\0\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ", "\0\0!@#$%^&*()_+\0\0QWERTYUIOP{}\0\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 "],
    ["\0\x001234567890-=\0\0йцукенгшщзхъ\0\0фывапролджэё\0\\ячсмитьбю.\0*\0 ", "\0\0!\"№;%:?*()_+\0\0ЙЦУКЕНГШЩЗХЪ\0\0ФЫВАПРОЛДЖЭЁ\0/ЯЧСМИТЬБЮ,\0*\0 "],
];
const KEYPAD: &str = "789-456+1230.";

pub fn key(code: u16) -> Option<Key> {
    const DIGITS: [Key; 10] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9, Key::Num0];
    const TOP: [Key; 10] = [Key::Q, Key::W, Key::E, Key::R, Key::T, Key::Y, Key::U, Key::I, Key::O, Key::P];
    const HOME: [Key; 9] = [Key::A, Key::S, Key::D, Key::F, Key::G, Key::H, Key::J, Key::K, Key::L];
    const BOTTOM: [Key; 7] = [Key::Z, Key::X, Key::C, Key::V, Key::B, Key::N, Key::M];
    const FUNCTION: [Key; 10] = [Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10];
    const PAD: [Key; 13] = [Key::Num7, Key::Num8, Key::Num9, Key::Minus, Key::Num4, Key::Num5, Key::Num6, Key::Plus, Key::Num1, Key::Num2, Key::Num3, Key::Num0, Key::Period];

    let index = usize::from(code);
    Some(match code {
        1 => Key::Escape,
        2..=11 => DIGITS[index - 2],
        12 => Key::Minus,
        13 => Key::Equals,
        14 => Key::Backspace,
        15 => Key::Tab,
        16..=25 => TOP[index - 16],
        26 => Key::OpenBracket,
        27 => Key::CloseBracket,
        28 | 96 => Key::Enter,
        LEFT_CTRL => Key::ControlLeft,
        30..=38 => HOME[index - 30],
        39 => Key::Semicolon,
        40 => Key::Quote,
        41 => Key::Backtick,
        LEFT_SHIFT => Key::ShiftLeft,
        43 => Key::Backslash,
        44..=50 => BOTTOM[index - 44],
        51 => Key::Comma,
        52 => Key::Period,
        53 | 98 => Key::Slash,
        RIGHT_SHIFT => Key::ShiftRight,
        LEFT_ALT => Key::AltLeft,
        SPACE => Key::Space,
        CAPS_LOCK => Key::CapsLock,
        59..=68 => FUNCTION[index - 59],
        71..=83 => PAD[index - 71],
        87 => Key::F11,
        88 => Key::F12,
        RIGHT_CTRL => Key::ControlRight,
        RIGHT_ALT => Key::AltRight,
        102 => Key::Home,
        103 => Key::ArrowUp,
        104 => Key::PageUp,
        105 => Key::ArrowLeft,
        106 => Key::ArrowRight,
        107 => Key::End,
        108 => Key::ArrowDown,
        109 => Key::PageDown,
        110 => Key::Insert,
        111 => Key::Delete,
        LEFT_META => Key::SuperLeft,
        RIGHT_META => Key::SuperRight,
        _ => return None,
    })
}

pub fn character(code: u16, language: Language, shift: bool, caps: bool) -> Option<char> {
    let index = usize::from(code);
    let character = match code {
        71..=83 => KEYPAD.chars().nth(index - 71)?,
        98 => '/',
        _ => {
            let [plain, shifted] = LAYOUTS[language as usize];
            let character = plain.chars().nth(index)?;
            if shift ^ (caps && character.is_lowercase()) { shifted.chars().nth(index)? } else { character }
        }
    };
    (character != '\0').then_some(character)
}

/// Follows a hardware keyboard: modifiers, caps lock, shortcuts and key repeat.
#[derive(Default)]
pub struct Physical {
    held: [bool; MODIFIER_KEYS.len()],
    modifiers: Modifiers,
    caps: bool,
    repeating: Option<(u16, f64)>,
}

impl Physical {
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// Returns whether the key means anything to the interface.
    pub fn press(&mut self, code: u16, pressed: bool, time: f64, language: &mut Language, events: &mut Vec<Event>) -> bool {
        if let Some(index) = MODIFIER_KEYS.iter().position(|&modifier| modifier == code) {
            self.held[index] = pressed;
            let [shift, alt, command] = [0..2, 2..4, 4..8].map(|keys| self.held[keys].contains(&true));
            self.modifiers = Modifiers { alt, ctrl: command, shift, mac_cmd: false, command };
            events.push(Event::ModifiersChanged(self.modifiers));
            if pressed && alt && shift {
                language.toggle();
            }
        } else if pressed {
            match code {
                CAPS_LOCK => self.caps = !self.caps,
                SPACE if self.modifiers.command => {
                    language.toggle();
                    return true;
                }
                _ => {}
            }
            self.repeating = Some((code, time + REPEAT_DELAY));
        } else if self.repeating.is_some_and(|(held, _)| held == code) {
            self.repeating = None;
        }
        self.emit(code, pressed, false, *language, events)
    }

    /// Repeats the held key. Returns when it is due next.
    pub fn tick(&mut self, time: f64, language: Language, events: &mut Vec<Event>) -> Option<f64> {
        let (code, due) = self.repeating?;
        if due > time {
            return Some(due);
        }
        self.repeating = self.emit(code, true, true, language, events).then_some((code, time + REPEAT_INTERVAL));
        self.repeating.map(|(_, due)| due)
    }

    /// A paste is sent without its text, which the clipboard's owner fills in.
    fn emit(&self, code: u16, pressed: bool, repeat: bool, language: Language, events: &mut Vec<Event>) -> bool {
        let Some(key) = key(code) else { return false };
        let modifiers = self.modifiers;
        let shortcut = match key {
            Key::C => Some(Event::Copy),
            Key::X => Some(Event::Cut),
            Key::V => Some(Event::Paste(String::new())),
            _ => None,
        };
        if let (true, true, Some(shortcut)) = (pressed, modifiers.command, shortcut) {
            events.push(shortcut);
            return true;
        }
        events.push(Event::Key { key, physical_key: Some(key), pressed, repeat, modifiers });
        if pressed
            && !modifiers.command
            && !modifiers.alt
            && let Some(character) = character(code, language, modifiers.shift, self.caps)
        {
            events.push(Event::Text(character.to_string()));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH: Language = Language::English;

    fn typed(events: &[Event]) -> String {
        events.iter().filter_map(|event| if let Event::Text(text) = event { Some(text.as_str()) } else { None }).collect()
    }

    #[test]
    fn characters_follow_language_shift_and_caps() {
        assert_eq!(character(30, ENGLISH, false, false), Some('a'));
        assert_eq!(character(30, ENGLISH, true, false), Some('A'));
        assert_eq!(character(30, ENGLISH, false, true), Some('A'));
        assert_eq!(character(30, ENGLISH, true, true), Some('a'));
        assert_eq!(character(2, ENGLISH, true, true), Some('!'));
        assert_eq!(character(2, ENGLISH, false, true), Some('1'));
        assert_eq!(character(53, ENGLISH, true, false), Some('?'));
        assert_eq!(character(57, ENGLISH, false, false), Some(' '));
        assert_eq!(character(79, ENGLISH, false, false), Some('1'));
        assert_eq!(character(28, ENGLISH, false, false), None);
        assert_eq!(character(400, ENGLISH, false, false), None);
        assert_eq!(character(16, Language::Russian, false, false), Some('й'));
        assert_eq!(character(41, Language::Russian, true, false), Some('Ё'));
        assert_eq!(character(52, Language::Russian, false, true), Some('Ю'));
        assert_eq!(character(53, Language::Russian, true, false), Some(','));
        assert_eq!((key(16), key(50), key(103), key(999)), (Some(Key::Q), Some(Key::M), Some(Key::ArrowUp), None));
        for [plain, shifted] in LAYOUTS {
            assert_eq!((plain.chars().count(), shifted.chars().count()), (58, 58));
        }
    }

    #[test]
    fn typing_produces_key_and_text_events() {
        let (mut keyboard, mut language, mut events) = (Physical::default(), ENGLISH, Vec::new());
        for (code, pressed) in [(LEFT_SHIFT, true), (35, true), (35, false), (LEFT_SHIFT, false), (23, true), (23, false)] {
            assert!(keyboard.press(code, pressed, 0.0, &mut language, &mut events));
        }
        assert_eq!(typed(&events), "Hi");
        assert!(matches!(events[0], Event::ModifiersChanged(modifiers) if modifiers.shift));
        assert!(events.iter().any(|event| matches!(event, Event::Key { key: Key::H, pressed: true, modifiers, .. } if modifiers.shift)));
        assert!(!keyboard.press(114, true, 0.0, &mut language, &mut events));
    }

    #[test]
    fn a_modifier_stays_down_while_either_of_its_keys_is_held() {
        let (mut keyboard, mut language, mut events) = (Physical::default(), ENGLISH, Vec::new());
        keyboard.press(LEFT_CTRL, true, 0.0, &mut language, &mut events);
        keyboard.press(RIGHT_CTRL, true, 0.0, &mut language, &mut events);
        keyboard.press(LEFT_CTRL, false, 0.0, &mut language, &mut events);
        assert!(keyboard.modifiers().command);
        keyboard.press(RIGHT_CTRL, false, 0.0, &mut language, &mut events);
        assert_eq!(keyboard.modifiers(), Modifiers::NONE);
    }

    #[test]
    fn control_shortcuts_copy_paste_and_switch_language() {
        let (mut keyboard, mut language, mut events) = (Physical::default(), ENGLISH, Vec::new());
        keyboard.press(LEFT_CTRL, true, 0.0, &mut language, &mut events);
        keyboard.press(47, true, 0.0, &mut language, &mut events);
        keyboard.press(46, true, 0.0, &mut language, &mut events);
        assert_eq!(events[2..], [Event::Paste(String::new()), Event::Copy]);

        keyboard.press(SPACE, true, 0.0, &mut language, &mut events);
        keyboard.press(LEFT_CTRL, false, 0.0, &mut language, &mut events);
        assert_eq!(language, Language::Russian);
        events.clear();
        keyboard.press(33, true, 0.0, &mut language, &mut events);
        assert_eq!(typed(&events), "а");
    }

    #[test]
    fn a_held_key_repeats_until_released() {
        let (mut keyboard, mut language, mut events) = (Physical::default(), ENGLISH, Vec::new());
        keyboard.press(30, true, 1.0, &mut language, &mut events);
        assert_eq!(keyboard.tick(1.1, language, &mut events), Some(1.0 + REPEAT_DELAY));
        assert_eq!(typed(&events), "a");

        let next = keyboard.tick(1.0 + REPEAT_DELAY, language, &mut events).unwrap();
        keyboard.tick(next, language, &mut events);
        assert_eq!(typed(&events), "aaa");
        assert!(events.iter().any(|event| matches!(event, Event::Key { repeat: true, pressed: true, .. })));

        keyboard.press(30, false, 2.0, &mut language, &mut events);
        assert_eq!(keyboard.tick(3.0, language, &mut events), None);
        assert_eq!(typed(&events), "aaa");
    }
}

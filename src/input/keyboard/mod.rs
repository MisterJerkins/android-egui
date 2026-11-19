//! Text entry: the keyboard as the interface has it, the hardware keyboard behind it, and
//! the clipboard. The on-screen keys are in `screen`.

mod screen;

use egui::emath::TSTransform;
use egui::{Context, Event, Id, Key, Modifiers, Order, OutputCommand, PlatformOutput, Pos2, Rect, vec2};

use screen::Screen;

use super::touch::Phase;

const REPEAT_DELAY: f64 = 0.4;
const REPEAT_INTERVAL: f64 = 0.05;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    English,
    Russian,
}

impl Language {
    fn toggle(&mut self) {
        *self = match self {
            Self::English => Self::Russian,
            Self::Russian => Self::English,
        };
    }
}

/// A clipboard outside the interface, which copied text is shared with.
pub(crate) trait Clipboard: Send {
    /// A copy or a paste is likely soon; getting ready now saves the first one a wait.
    fn prepare(&mut self) {}
    /// Called now and then, to give up what has not been needed for a while.
    fn rest(&mut self) {}
    fn read(&mut self) -> Option<String>;
    fn write(&mut self, text: &str);
}

/// Text entry: a hardware keyboard, plus an on-screen one that shows up while a text field has focus.
pub struct Keyboard {
    language: Language,
    physical: Physical,
    screen: Screen,
    clipboard: String,
    system: Option<Box<dyn Clipboard>>,
    on_screen: bool,
    wanted: bool,
    typed: bool,
    /// The text field the keyboard came up for.
    field: Option<Id>,
    /// How far the interface is moved up to keep that field above the keys, in points.
    lift: f32,
    /// Whether the field sits on the background, which then moves up with the rest.
    grounded: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            language: Language::default(),
            physical: Physical::default(),
            screen: Screen::default(),
            clipboard: String::new(),
            system: None,
            on_screen: true,
            wanted: false,
            typed: false,
            field: None,
            lift: 0.0,
            grounded: false,
        }
    }
}

impl Keyboard {
    pub fn language(&self) -> Language {
        self.language
    }

    pub fn set_language(&mut self, language: Language) {
        self.language = language;
    }

    /// Whether the on-screen keyboard may appear. A hardware keyboard works either way.
    pub fn set_on_screen(&mut self, enabled: bool) {
        self.on_screen = enabled;
    }

    /// The text on the clipboard: the system's where it can be asked, else what was last copied here.
    pub fn clipboard(&mut self) -> &str {
        if let Some(text) = self.system.as_mut().and_then(|system| system.read()) {
            self.clipboard = text;
        }
        &self.clipboard
    }

    /// Copies `text`, as the interface does on Ctrl+C.
    pub fn set_clipboard(&mut self, text: impl Into<String>) {
        self.clipboard = text.into();
        if let Some(system) = &mut self.system {
            system.write(&self.clipboard);
        }
    }

    pub(crate) fn share_clipboard(&mut self, system: Box<dyn Clipboard>) {
        self.system = Some(system);
    }

    /// Called now and then between frames.
    pub(crate) fn rest(&mut self) {
        if let Some(system) = &mut self.system {
            system.rest();
        }
    }

    /// Where the on-screen keyboard is, in points, while it is shown.
    pub fn rect(&self) -> Option<Rect> {
        self.screen.bounds()
    }

    /// The on-screen keys in points, named by the character they type or by what they do.
    pub fn keys(&self) -> impl Iterator<Item = (String, Rect)> + '_ {
        self.screen.keys()
    }

    pub(crate) fn modifiers(&self) -> Modifiers {
        self.physical.modifiers()
    }

    pub(crate) fn key(&mut self, code: u16, pressed: bool, time: f64, events: &mut Vec<Event>) {
        let from = events.len();
        let used = self.physical.press(code, pressed, time, &mut self.language, events);
        self.typed |= used && pressed && self.wanted;
        self.paste(&mut events[from..], self.physical.modifiers().command);
    }

    pub(crate) fn touch(&mut self, touch: u64, phase: Phase, position: Pos2, time: f64, events: &mut Vec<Event>) {
        let from = events.len();
        self.screen.touch(touch, phase, position, time, &mut self.language, events);
        self.paste(&mut events[from..], self.screen.is_editing());
    }

    /// Key repeat. Returns when it is due next.
    pub(crate) fn tick(&mut self, time: f64, events: &mut Vec<Event>) -> Option<f64> {
        let from = events.len();
        let physical = self.physical.tick(time, self.language, events);
        let screen = self.screen.tick(time, &mut self.language, events);
        self.paste(&mut events[from..], false);
        [physical, screen].into_iter().flatten().reduce(f64::min)
    }

    /// Gives the paste events among `events` their text. `likely` says a copy or paste may come next.
    fn paste(&mut self, events: &mut [Event], likely: bool) {
        let mut pastes = events.iter_mut().filter_map(|event| if let Event::Paste(text) = event { Some(text) } else { None }).peekable();
        if pastes.peek().is_some() {
            let text = self.clipboard();
            pastes.for_each(|pasted| *pasted = text.to_owned());
        } else if likely && let Some(system) = &mut self.system {
            system.prepare();
        }
    }

    /// Places the on-screen keyboard for the coming frame.
    pub(crate) fn arrange(&mut self, viewport: Rect, zoom: f32) {
        self.screen.arrange(self.on_screen && self.wanted && !self.typed, self.language, viewport, zoom);
    }

    pub(crate) fn paint(&self, context: &Context) {
        self.screen.paint(context);
    }

    /// Takes what the frame asked of the platform. Returns whether another frame is due: the
    /// on-screen keyboard has to appear or go, or the interface has to move out of its way.
    pub(crate) fn finish(&mut self, context: &Context, output: &mut PlatformOutput) -> bool {
        for command in output.commands.drain(..) {
            if let OutputCommand::CopyText(text) = command {
                self.set_clipboard(text);
            }
        }
        // A field asks for text only while it is in sight. Once the keyboard is up for one, it
        // stays for as long as that field keeps the focus, seen or not.
        let focused = context.memory(|memory| memory.focused());
        if output.ime.is_some() {
            self.field = focused;
        }
        let wanted = self.field.is_some() && self.field == focused;
        let changed = wanted != self.wanted;
        if changed {
            (self.wanted, self.typed) = (wanted, false);
        }
        if !wanted {
            self.field = None;
        }
        let moved = self.raise(context, output.ime.as_ref().map(|field| field.cursor_rect));
        changed || moved || self.screen.bounds().is_some() != (self.on_screen && self.wanted && !self.typed)
    }

    /// Moves the interface up by as much as the line being typed is under the keys, and back
    /// once it need not be. `cursor` is where that line was drawn. Returns whether anything moved.
    fn raise(&mut self, context: &Context, cursor: Option<Rect>) -> bool {
        let pixel = 1.0 / context.pixels_per_point();
        let lift = match (self.screen.bounds(), cursor) {
            (Some(keys), Some(cursor)) => {
                // The line is reported where it was drawn, the lift already taken off. Eight points
                // of room are kept between the line and the keys under it.
                let needed = (cursor.bottom() + self.lift + 8.0 - keys.top()).max(0.0);
                if (needed - self.lift).abs() <= pixel { self.lift } else { (needed / pixel).round() * pixel }
            }
            (Some(_), None) => self.lift,
            (None, _) => 0.0,
        };
        let moved = lift != self.lift;
        if lift > 0.0 && self.lift == 0.0 {
            let home = self.field.and_then(|field| context.read_response(field)).map(|field| field.layer_id.order);
            self.grounded = home == Some(Order::Background);
        }
        // New layers, a menu that drops down from a lifted window, are lifted as they come.
        if moved || lift > 0.0 {
            let up = TSTransform::from_translation(vec2(0.0, -lift));
            for layer in context.memory(|memory| memory.areas().visible_layer_ids()) {
                if layer.order != Order::Background || self.grounded {
                    context.set_transform_layer(layer, up);
                }
            }
        }
        self.lift = lift;
        moved
    }
}

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

fn key(code: u16) -> Option<Key> {
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

fn character(code: u16, language: Language, shift: bool, caps: bool) -> Option<char> {
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
struct Physical {
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
    use egui::{RawInput, TextEdit, pos2};

    use super::*;

    struct Harness {
        context: Context,
        keyboard: Keyboard,
        text: String,
        time: f64,
        /// How far down its window the text field sits; zero puts it at the top of the screen.
        depth: f32,
        /// Where the field was drawn last, on screen.
        field: Rect,
    }

    impl Harness {
        /// A text field that takes focus on the first frame, run until the keyboard has settled.
        fn new() -> Self {
            let mut harness = Self::with_field_at(0.0);
            assert!(!harness.frame(Vec::new()) && harness.keyboard.rect().is_none());
            assert!(harness.frame(Vec::new()));
            assert!(!harness.frame(Vec::new()));
            harness
        }

        fn with_field_at(depth: f32) -> Self {
            Self { context: Context::default(), keyboard: Keyboard::default(), text: String::new(), time: 0.0, depth, field: Rect::NOTHING }
        }

        fn frame(&mut self, mut events: Vec<Event>) -> bool {
            self.time += 1.0 / 60.0;
            let viewport = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 600.0));
            self.keyboard.tick(self.time, &mut events);
            self.keyboard.arrange(viewport, 1.0);
            let input = RawInput { screen_rect: Some(viewport), time: Some(self.time), events, ..Default::default() };
            let (keyboard, text, first, depth, seen) = (&self.keyboard, &mut self.text, self.time < 0.02, self.depth, &mut self.field);
            let mut output = self.context.run_ui(input, |ui| {
                let mut field = |ui: &mut egui::Ui| {
                    ui.add_space(depth);
                    let field = ui.add(TextEdit::singleline(text));
                    if first {
                        field.request_focus();
                    }
                    *seen = ui.ctx().layer_transform_to_global(ui.layer_id()).unwrap_or_default() * field.rect;
                };
                if depth > 0.0 {
                    egui::Window::new("tall").fixed_pos(pos2(100.0, 20.0)).show(ui.ctx(), field);
                } else {
                    field(ui);
                }
                keyboard.paint(ui.ctx());
            });
            output.textures_delta.clear();
            self.keyboard.finish(&self.context, &mut output.platform_output)
        }

        fn tap(&mut self, name: &str) {
            let position = self.keyboard.keys().find(|(key, _)| key == name).unwrap_or_else(|| panic!("no key '{name}'")).1.center();
            let mut events = Vec::new();
            self.keyboard.touch(0, Phase::Start, position, self.time, &mut events);
            self.keyboard.touch(0, Phase::End, position, self.time, &mut events);
            self.frame(events);
        }

        fn press(&mut self, code: u16) {
            let mut events = Vec::new();
            self.keyboard.key(code, true, self.time, &mut events);
            self.keyboard.key(code, false, self.time, &mut events);
            self.frame(events);
        }
    }

    #[test]
    fn the_on_screen_keyboard_follows_focus_and_types_into_the_field() {
        let mut harness = Harness::new();
        let rect = harness.keyboard.rect().expect("a focused text field brings the keyboard up");
        assert!(harness.field.bottom() < rect.top() && harness.field.top() >= 0.0, "a field clear of the keys stays where it is: {:?}", harness.field);

        for key in ["shift", "O", "k", "symbols", "1", "backspace", "2"] {
            harness.tap(key);
        }
        assert_eq!(harness.text, "Ok2");
        harness.tap("enter");
        assert!(harness.context.memory(|memory| memory.focused().is_none()));
    }

    #[test]
    fn a_hardware_keyboard_types_and_puts_the_on_screen_one_away() {
        let mut harness = Harness::new();
        assert!(harness.keyboard.rect().is_some());

        harness.press(35);
        harness.press(23);
        assert_eq!(harness.text, "hi");
        assert!(harness.keyboard.rect().is_none());
        assert_eq!(harness.context.content_rect().bottom(), 600.0);
    }

    #[test]
    fn a_field_the_keyboard_would_cover_is_lifted_into_view_and_the_keyboard_stays() {
        let mut harness = Harness::with_field_at(450.0);
        let mut settled = 0;
        let shown: Vec<bool> = (0..40)
            .map(|frame| {
                if harness.frame(Vec::new()) {
                    settled = frame + 1;
                }
                harness.keyboard.rect().is_some()
            })
            .collect();
        let appeared = shown.iter().position(|&shown| shown).expect("the keyboard comes up");
        assert!(shown[appeared..].iter().all(|&shown| shown), "the keyboard came and went: {shown:?}");
        assert!(settled <= appeared + 2, "frames were still asked for at frame {settled}");

        let keys = harness.keyboard.rect().unwrap();
        assert!(harness.field.bottom() <= keys.top(), "the field at {:?} is under the keys at {keys:?}", harness.field);
        assert!(harness.field.top() >= 0.0);
        harness.tap("x");
        assert_eq!(harness.text, "x");

        harness.tap("enter");
        harness.frame(Vec::new());
        harness.frame(Vec::new());
        assert!(harness.keyboard.rect().is_none());
        assert!(harness.field.top() > keys.top(), "the window is back where it was: {:?}", harness.field);
    }

    /// A stand-in for the system clipboard, shared with the test that looks into it.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<(String, u32)>>);

    impl Clipboard for Shared {
        fn prepare(&mut self) {
            self.0.lock().unwrap().1 += 1;
        }

        fn read(&mut self) -> Option<String> {
            Some(self.0.lock().unwrap().0.clone()).filter(|text| !text.is_empty())
        }

        fn write(&mut self, text: &str) {
            self.0.lock().unwrap().0 = text.to_owned();
        }
    }

    #[test]
    fn the_editing_page_selects_cuts_and_pastes_through_the_shared_clipboard() {
        let mut harness = Harness::new();
        let system = Shared::default();
        harness.keyboard.share_clipboard(Box::new(system.clone()));
        system.0.lock().unwrap().0 = "from outside".into();

        for key in ["symbols", "more"] {
            harness.tap(key);
        }
        assert_eq!(system.0.lock().unwrap().1, 0, "nothing asks for the clipboard before its keys are in sight");
        harness.tap("more");
        assert!(system.0.lock().unwrap().1 > 0);

        harness.tap("paste");
        assert_eq!(harness.text, "from outside");
        for key in ["home", "right", "delete", "end", "left", "backspace"] {
            harness.tap(key);
        }
        assert_eq!(harness.text, "fom outsie");

        // The field only remembers a text for undoing once it has been left alone for a moment.
        harness.time += 2.0;
        harness.frame(Vec::new());
        harness.tap("all");
        harness.tap("cut");
        assert_eq!((harness.text.as_str(), system.0.lock().unwrap().0.as_str()), ("", "fom outsie"));
        harness.tap("undo");
        assert_eq!(harness.text, "fom outsie");

        system.0.lock().unwrap().0.clear();
        assert_eq!(harness.keyboard.clipboard(), "fom outsie", "what was copied here stays when the system has no text");
        harness.keyboard.set_clipboard("set from code");
        assert_eq!(system.0.lock().unwrap().0, "set from code");
    }

    #[test]
    fn a_hardware_paste_takes_the_shared_clipboard_and_a_copy_goes_to_it() {
        let mut harness = Harness::new();
        let system = Shared::default();
        harness.keyboard.share_clipboard(Box::new(system.clone()));
        system.0.lock().unwrap().0 = "one".into();

        let mut events = Vec::new();
        for (code, pressed) in [(29, true), (47, true), (47, false), (30, true), (30, false), (46, true), (46, false), (29, false)] {
            if code == 30 && pressed {
                system.0.lock().unwrap().0 = "changed elsewhere".into();
            }
            harness.keyboard.key(code, pressed, harness.time, &mut events);
            harness.frame(std::mem::take(&mut events));
        }
        assert_eq!(harness.text, "one");
        assert_eq!(system.0.lock().unwrap().0, "one", "copying the selection replaces what was there");
        assert!(system.0.lock().unwrap().1 > 0);
    }

    #[test]
    fn copied_text_comes_back_on_paste() {
        let mut harness = Harness::new();
        harness.press(30);
        harness.press(48);

        let mut events = Vec::new();
        for (code, pressed) in [(29, true), (30, true), (30, false), (46, true), (46, false), (47, true), (47, false), (47, true), (47, false), (29, false)] {
            harness.keyboard.key(code, pressed, harness.time, &mut events);
            harness.frame(std::mem::take(&mut events));
        }
        assert_eq!(harness.keyboard.clipboard(), "ab");
        assert_eq!(harness.text, "abab");
    }

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

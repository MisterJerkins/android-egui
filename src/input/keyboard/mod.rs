mod physical;
mod screen;

use egui::emath::TSTransform;
use egui::{Context, Event, Id, Modifiers, Order, OutputCommand, PlatformOutput, Pos2, Rect, vec2};

use crate::input::Phase;
use physical::Physical;
use screen::Screen;

const REPEAT_DELAY: f64 = 0.4;
const REPEAT_INTERVAL: f64 = 0.05;
/// Room kept between the line being typed and the keys under it, in points.
const CLEARANCE: f32 = 8.0;

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
                // The line is reported where it was drawn, the lift already taken off.
                let needed = (cursor.bottom() + self.lift + CLEARANCE - keys.top()).max(0.0);
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
}

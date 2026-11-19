use egui::{Context, Event, FontId, Id, Key, LayerId, Modifiers, Order, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};

use super::{Language, REPEAT_DELAY, REPEAT_INTERVAL};
use crate::input::touch::Phase;

const ROWS: f32 = 4.0;
const PLAIN: Modifiers = Modifiers::NONE;
/// Ctrl, the way a hardware keyboard reports it.
const COMMAND: Modifiers = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };

#[derive(Clone, Copy, Default, PartialEq)]
enum Page {
    #[default]
    Letters,
    Symbols,
    More,
    Edit,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Case {
    #[default]
    Lower,
    Upper,
    Locked,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cap {
    Char(char),
    Space,
    Enter,
    Backspace,
    Shift,
    Symbols,
    More,
    Language,
    /// A key press passed on as it is: moving the cursor, and shortcuts.
    Key(Key, Modifiers),
    Cut,
    Copy,
    Paste,
}

impl Cap {
    /// Whether the key acts again and again while it is held.
    fn repeats(self) -> bool {
        matches!(self, Self::Backspace) || matches!(self, Self::Key(_, modifiers) if modifiers.is_none())
    }
}

struct Button {
    rect: Rect,
    cap: Cap,
}

struct Held {
    touch: u64,
    button: Option<usize>,
    repeat: Option<f64>,
    spent: bool,
}

/// The on-screen keyboard. It is painted on its own layer and fed touches directly, so typing
/// never moves the pointer and the text field keeps its focus.
#[derive(Default)]
pub struct Screen {
    page: Page,
    case: Case,
    language: Language,
    bounds: Option<Rect>,
    buttons: Vec<Button>,
    held: Vec<Held>,
}

impl Screen {
    pub fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    /// Whether the page with the clipboard keys is open.
    pub fn is_editing(&self) -> bool {
        self.page == Page::Edit
    }

    pub fn keys(&self) -> impl Iterator<Item = (String, Rect)> + '_ {
        let name = |cap| match cap {
            Cap::Char(character) => self.cased(character).to_string(),
            Cap::Space => "space".into(),
            Cap::Enter => "enter".into(),
            Cap::Backspace => "backspace".into(),
            Cap::Shift => "shift".into(),
            Cap::Symbols => "symbols".into(),
            Cap::More => "more".into(),
            Cap::Language => "language".into(),
            Cap::Key(Key::A, _) => "all".into(),
            Cap::Key(Key::Z, modifiers) => if modifiers.shift { "redo" } else { "undo" }.into(),
            Cap::Key(key, _) => key.name().to_lowercase(),
            Cap::Cut => "cut".into(),
            Cap::Copy => "copy".into(),
            Cap::Paste => "paste".into(),
        };
        self.buttons.iter().map(move |button| (name(button.cap), button.rect))
    }

    /// Lays the keyboard out along the bottom edge and returns its height, zero while hidden.
    pub fn arrange(&mut self, shown: bool, language: Language, viewport: Rect, zoom: f32) -> f32 {
        if !shown {
            *self = Self::default();
            return 0.0;
        }
        // 840 points wide and 52 a row before zoom, and no more than the screen's width and half its height.
        let size = vec2((840.0 / zoom).min(viewport.width()), (ROWS * 52.0 / zoom).min(viewport.height() / 2.0));
        let bounds = Rect::from_min_size(pos2(viewport.center().x - size.x / 2.0, viewport.bottom() - size.y), size);
        if self.bounds != Some(bounds) || self.language != language {
            (self.bounds, self.language) = (Some(bounds), language);
            self.rebuild();
        }
        size.y
    }

    pub fn touch(&mut self, touch: u64, phase: Phase, position: Pos2, time: f64, language: &mut Language, events: &mut Vec<Event>) {
        let under = self.button_at(position);
        let slot = self.held.iter().position(|held| held.touch == touch);
        match (phase, slot) {
            (Phase::Start, _) => {
                for slot in 0..self.held.len() {
                    self.release(slot, language, events);
                }
                let cap = under.map(|index| self.buttons[index].cap);
                let instant = cap.filter(|cap| !matches!(cap, Cap::Char(_) | Cap::Space | Cap::Enter));
                if let Some(cap) = instant {
                    self.press(cap, false, language, events);
                }
                let repeat = cap.is_some_and(Cap::repeats).then_some(time + REPEAT_DELAY);
                self.held.push(Held { touch, button: under, repeat, spent: instant.is_some() });
            }
            (Phase::Move, Some(slot)) if !self.held[slot].spent => self.held[slot].button = under,
            (Phase::End, Some(slot)) => {
                if !self.held[slot].spent {
                    self.held[slot].button = under;
                }
                self.release(slot, language, events);
                self.held.remove(slot);
            }
            (Phase::Cancel, Some(slot)) => drop(self.held.remove(slot)),
            _ => {}
        }
    }

    /// Repeats a held backspace or cursor key. Returns when it is due next.
    pub fn tick(&mut self, time: f64, language: &mut Language, events: &mut Vec<Event>) -> Option<f64> {
        let mut next = None::<f64>;
        for slot in 0..self.held.len() {
            let held = &self.held[slot];
            let (Some(mut due), Some(button)) = (held.repeat, held.button.and_then(|index| self.buttons.get(index))) else { continue };
            if due <= time {
                due = time + REPEAT_INTERVAL;
                self.held[slot].repeat = Some(due);
                self.press(button.cap, true, language, events);
            }
            next = Some(next.map_or(due, |next| next.min(due)));
        }
        next
    }

    pub fn paint(&self, context: &Context) {
        let Some(bounds) = self.bounds else { return };
        let painter = Painter::new(context.clone(), LayerId::new(Order::Tooltip, Id::unique("android-egui-keyboard")), context.viewport_rect());
        let style = context.global_style();
        let visuals = &style.visuals;
        let unit = bounds.height() / ROWS;
        painter.rect_filled(bounds, 0.0, visuals.panel_fill);
        for (index, button) in self.buttons.iter().enumerate() {
            let lit = self.held.iter().any(|held| held.button == Some(index)) || (button.cap == Cap::Shift && self.case != Case::Lower);
            let (fill, ink) = if lit { (visuals.selection.bg_fill, visuals.strong_text_color()) } else { (visuals.widgets.inactive.bg_fill, visuals.text_color()) };
            let rect = button.rect.shrink(unit * 0.06);
            painter.rect_filled(rect, unit * 0.14, fill);

            let label = self.label(button.cap);
            if label.is_empty() {
                icon(&painter, button.cap, rect.center(), unit * 0.2, Stroke::new(unit * 0.045, ink), self.case == Case::Locked);
            } else {
                let size = unit * if label.chars().count() > 1 { 0.3 } else { 0.42 };
                let room = rect.width() * 0.9;
                let mut text = painter.layout_no_wrap(label.to_owned(), FontId::proportional(size), ink);
                if text.size().x > room {
                    text = painter.layout_no_wrap(label.to_owned(), FontId::proportional(size * room / text.size().x), ink);
                }
                painter.galley(rect.center() - text.size() / 2.0, text, ink);
            }
        }
    }

    fn rebuild(&mut self) {
        self.buttons = self.bounds.map(|bounds| layout(self.page, self.language, bounds)).unwrap_or_default();
        self.held.iter_mut().for_each(|held| held.button = None);
    }

    fn button_at(&self, position: Pos2) -> Option<usize> {
        self.bounds.filter(|bounds| bounds.contains(position))?;
        let distance = |index: &usize| self.buttons[*index].rect.distance_sq_to_pos(position);
        (0..self.buttons.len()).min_by(|a, b| distance(a).total_cmp(&distance(b)))
    }

    fn release(&mut self, slot: usize, language: &mut Language, events: &mut Vec<Event>) {
        let held = &mut self.held[slot];
        if std::mem::replace(&mut held.spent, true) {
            return;
        }
        if let Some(cap) = held.button.and_then(|index| self.buttons.get(index)).map(|button| button.cap) {
            self.press(cap, false, language, events);
        }
    }

    fn press(&mut self, cap: Cap, repeat: bool, language: &mut Language, events: &mut Vec<Event>) {
        let mut key = |key, modifiers| {
            for pressed in [true, false] {
                events.push(Event::Key { key, physical_key: None, pressed, repeat: repeat && pressed, modifiers });
            }
        };
        match cap {
            Cap::Enter => key(Key::Enter, PLAIN),
            Cap::Backspace => key(Key::Backspace, PLAIN),
            Cap::Key(pressed, modifiers) => key(pressed, modifiers),
            Cap::Cut => events.push(Event::Cut),
            Cap::Copy => events.push(Event::Copy),
            // Sent without its text, which the clipboard's owner fills in.
            Cap::Paste => events.push(Event::Paste(String::new())),
            Cap::Space => events.push(Event::Text(" ".into())),
            Cap::Char(character) => {
                events.push(Event::Text(self.cased(character).to_string()));
                if self.case == Case::Upper {
                    self.case = Case::Lower;
                }
            }
            Cap::Shift => {
                self.case = match self.case {
                    Case::Lower => Case::Upper,
                    Case::Upper => Case::Locked,
                    Case::Locked => Case::Lower,
                }
            }
            Cap::Symbols | Cap::More | Cap::Language => {
                match cap {
                    Cap::Symbols => self.page = if self.page == Page::Letters { Page::Symbols } else { Page::Letters },
                    Cap::More => {
                        self.page = match self.page {
                            Page::Symbols => Page::More,
                            Page::More => Page::Edit,
                            _ => Page::Symbols,
                        }
                    }
                    _ => language.toggle(),
                }
                self.language = *language;
                self.rebuild();
            }
        }
    }

    fn cased(&self, character: char) -> char {
        if self.case == Case::Lower || self.page != Page::Letters { character } else { character.to_uppercase().next().unwrap_or(character) }
    }

    /// What is written on a key. Keys without words on them get an icon.
    fn label(&self, cap: Cap) -> String {
        let words = |english: &'static str, russian: &'static str| if self.language == Language::English { english } else { russian };
        let text = match (cap, self.page) {
            (Cap::Char(character), _) => return self.cased(character).to_string(),
            (Cap::Symbols, Page::Letters) => "?123",
            (Cap::Symbols, _) => words("ABC", "АБВ"),
            (Cap::More, Page::Symbols) => "1/3",
            (Cap::More, Page::More) => "2/3",
            (Cap::More, _) => "3/3",
            (Cap::Language, _) => words("EN", "RU"),
            (Cap::Key(Key::A, _), _) => words("Select all", "Выделить всё"),
            (Cap::Key(Key::Z, modifiers), _) if modifiers.shift => words("Redo", "Вернуть"),
            (Cap::Key(Key::Z, _), _) => words("Undo", "Отменить"),
            (Cap::Key(Key::Home, _), _) => "Home",
            (Cap::Key(Key::End, _), _) => "End",
            (Cap::Key(Key::Delete, _), _) => "Del",
            (Cap::Cut, _) => words("Cut", "Вырезать"),
            (Cap::Copy, _) => words("Copy", "Копировать"),
            (Cap::Paste, _) => words("Paste", "Вставить"),
            _ => "",
        };
        text.into()
    }
}

fn layout(page: Page, language: Language, bounds: Rect) -> Vec<Button> {
    let chars = |row: &str| row.chars().map(Cap::Char).collect::<Vec<_>>();
    let [first, second, mut third] = match (page, language) {
        (Page::Letters, Language::English) => ["qwertyuiop", "asdfghjkl", "zxcvbnm"].map(chars),
        (Page::Letters, Language::Russian) => ["йцукенгшщзхъ", "фывапролджэё", "ячсмитьбю"].map(chars),
        (Page::Symbols, _) => ["1234567890", "@#$%&-+()/", "*\"':;!?"].map(chars),
        (Page::More, _) => ["~`|^=_{}[]", "<>\\€£¥°•√π", "÷×§©®™…"].map(chars),
        (Page::Edit, _) => [
            vec![Cap::Key(Key::A, COMMAND), Cap::Cut, Cap::Copy, Cap::Paste],
            vec![
                Cap::Key(Key::Home, PLAIN),
                Cap::Key(Key::ArrowLeft, PLAIN),
                Cap::Key(Key::ArrowUp, PLAIN),
                Cap::Key(Key::ArrowDown, PLAIN),
                Cap::Key(Key::ArrowRight, PLAIN),
                Cap::Key(Key::End, PLAIN),
            ],
            vec![Cap::Key(Key::Z, COMMAND), Cap::Key(Key::Z, Modifiers { shift: true, ..COMMAND }), Cap::Key(Key::Delete, PLAIN)],
        ],
    };
    third.insert(0, if page == Page::Letters { Cap::Shift } else { Cap::More });
    third.push(Cap::Backspace);
    let rows = [first, second, third, vec![Cap::Symbols, Cap::Language, Cap::Char(','), Cap::Space, Cap::Char('.'), Cap::Enter]];

    // A character key is one unit wide and any other key one and a half; the space bar is
    // given what its row has left.
    let weight = |cap: &Cap| match cap {
        Cap::Char(_) => 1.0,
        Cap::Space => 0.0,
        _ => 1.5,
    };
    let total = |row: &Vec<Cap>| row.iter().map(weight).sum::<f32>();
    let units = rows.iter().map(total).fold(0.0, f32::max);
    let (unit, height) = (bounds.width() / units, bounds.height() / ROWS);

    let mut buttons = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let spare = units - total(row);
        // A row is filled by its space bar; without one it is centred, or on the editing page,
        // which has few keys, shared out between them.
        let (grow, margin) = match (row.contains(&Cap::Space), page) {
            (true, _) => (1.0, 0.0),
            (false, Page::Edit) => (units / total(row), 0.0),
            (false, _) => (1.0, spare * unit / 2.0),
        };
        let mut left = bounds.left() + margin;
        for &cap in row {
            let width = unit * if cap == Cap::Space { spare } else { weight(&cap) * grow };
            buttons.push(Button { rect: Rect::from_min_size(pos2(left, bounds.top() + index as f32 * height), vec2(width, height)), cap });
            left += width;
        }
    }
    buttons
}

fn icon(painter: &Painter, cap: Cap, center: Pos2, size: f32, stroke: Stroke, locked: bool) {
    let path = |points: &[[f32; 2]]| points.iter().map(|&[x, y]| center + vec2(x, y) * size).collect::<Vec<_>>();
    let shapes = match cap {
        Cap::Shift => vec![
            Shape::closed_line(path(&[[0.0, -1.0], [1.0, 0.0], [0.45, 0.0], [0.45, 0.7], [-0.45, 0.7], [-0.45, 0.0], [-1.0, 0.0]]), stroke),
            Shape::line(path(if locked { &[[-0.45, 1.1], [0.45, 1.1]] } else { &[] }), stroke),
        ],
        Cap::Backspace => vec![
            Shape::closed_line(path(&[[-1.3, 0.0], [-0.5, -0.8], [1.2, -0.8], [1.2, 0.8], [-0.5, 0.8]]), stroke),
            Shape::line(path(&[[0.0, -0.35], [0.7, 0.35]]), stroke),
            Shape::line(path(&[[0.7, -0.35], [0.0, 0.35]]), stroke),
        ],
        Cap::Enter => vec![Shape::line(path(&[[0.9, -0.7], [0.9, 0.2], [-0.9, 0.2]]), stroke), Shape::line(path(&[[-0.45, -0.25], [-0.9, 0.2], [-0.45, 0.65]]), stroke)],
        Cap::Space => vec![Shape::line(path(&[[-2.0, 0.0], [-2.0, 0.4], [2.0, 0.4], [2.0, 0.0]]), stroke)],
        Cap::Key(key @ (Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown), _) => {
            let turn = |[x, y]: [f32; 2]| match key {
                Key::ArrowLeft => [x, y],
                Key::ArrowRight => [-x, y],
                Key::ArrowUp => [y, x],
                _ => [y, -x],
            };
            vec![Shape::line(path(&[[0.9, 0.0], [-0.9, 0.0]].map(turn)), stroke), Shape::line(path(&[[-0.35, -0.55], [-0.9, 0.0], [-0.35, 0.55]].map(turn)), stroke)]
        }
        _ => Vec::new(),
    };
    painter.extend(shapes);
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 600.0));

    struct Typist {
        screen: Screen,
        language: Language,
        events: Vec<Event>,
    }

    impl Typist {
        fn new() -> Self {
            let mut typist = Self { screen: Screen::default(), language: Language::English, events: Vec::new() };
            typist.screen.arrange(true, typist.language, VIEWPORT, 1.0);
            typist
        }

        fn center(&self, name: &str) -> Pos2 {
            self.screen.keys().find(|(key, _)| key == name).unwrap_or_else(|| panic!("no key '{name}'")).1.center()
        }

        fn finger(&mut self, touch: u64, phase: Phase, name: &str, time: f64) {
            let position = self.center(name);
            self.screen.touch(touch, phase, position, time, &mut self.language, &mut self.events);
        }

        fn tap(&mut self, name: &str) {
            let position = self.center(name);
            for phase in [Phase::Start, Phase::End] {
                self.screen.touch(0, phase, position, 0.0, &mut self.language, &mut self.events);
            }
        }

        fn text(&self) -> String {
            super::super::typed(&self.events)
        }

        fn presses(&self, wanted: Key) -> usize {
            self.events.iter().filter(|event| matches!(event, Event::Key { key, pressed: true, .. } if *key == wanted)).count()
        }
    }

    #[test]
    fn every_page_tiles_the_keyboard_without_gaps_or_overlaps() {
        let bounds = Rect::from_min_size(pos2(80.0, 392.0), vec2(840.0, 208.0));
        let pages = [
            (Page::Letters, Language::English, 34),
            (Page::Letters, Language::Russian, 41),
            (Page::Symbols, Language::English, 35),
            (Page::More, Language::Russian, 35),
            (Page::Edit, Language::Russian, 21),
        ];
        for (page, language, count) in pages {
            let buttons = layout(page, language, bounds);
            assert_eq!(buttons.len(), count);
            for (index, button) in buttons.iter().enumerate() {
                assert!(bounds.expand(0.01).contains_rect(button.rect) && button.rect.width() > 40.0, "{:?} at {:?}", button.cap, button.rect);
                for other in &buttons[index + 1..] {
                    assert!(!button.rect.shrink(0.01).intersects(other.rect), "{:?} overlaps {:?}", button.cap, other.cap);
                }
            }
            let area: f32 = buttons.iter().map(|button| button.rect.area()).sum();
            let empty = if page == Page::Letters && language == Language::English { 52.0 * 84.0 } else { 0.0 };
            assert!((area + empty - bounds.area()).abs() < 1.0, "{area} of {}", bounds.area());
        }
    }

    #[test]
    fn the_keyboard_sits_at_the_bottom_and_ignores_zoom() {
        let mut screen = Screen::default();
        assert_eq!(screen.arrange(true, Language::English, VIEWPORT, 1.0), 208.0);
        assert_eq!(screen.bounds(), Some(Rect::from_min_max(pos2(80.0, 392.0), pos2(920.0, 600.0))));
        assert_eq!(screen.arrange(true, Language::English, Rect::from_min_max(pos2(0.0, 0.0), pos2(500.0, 300.0)), 2.0), 104.0);
        assert_eq!(screen.arrange(true, Language::English, Rect::from_min_max(pos2(0.0, 0.0), pos2(640.0, 360.0)), 1.0), 180.0);
        assert_eq!(screen.arrange(false, Language::English, VIEWPORT, 1.0), 0.0);
        assert_eq!((screen.bounds(), screen.keys().count()), (None, 0));
    }

    #[test]
    fn taps_type_text_and_shift_applies_once() {
        let mut typist = Typist::new();
        typist.tap("shift");
        for key in ["H", "i", "space", ",", "enter"] {
            typist.tap(key);
        }
        assert_eq!(typist.text(), "Hi ,");
        assert_eq!(typist.presses(Key::Enter), 1);

        typist.tap("shift");
        typist.tap("shift");
        typist.tap("A");
        typist.tap("B");
        assert_eq!(typist.text(), "Hi ,AB");
    }

    #[test]
    fn pages_and_languages_switch() {
        let mut typist = Typist::new();
        for key in ["symbols", "7", "more", "€", "symbols", "language"] {
            typist.tap(key);
        }
        assert_eq!(typist.language, Language::Russian);
        typist.tap("shift");
        typist.tap("Ж");
        typist.tap("ё");
        assert_eq!(typist.text(), "7€Жё");
    }

    #[test]
    fn a_second_finger_commits_the_first_key_and_sliding_picks_the_last_one() {
        let mut typist = Typist::new();
        typist.finger(1, Phase::Start, "a", 0.0);
        typist.finger(2, Phase::Start, "b", 0.0);
        assert_eq!(typist.text(), "a");
        typist.finger(1, Phase::End, "a", 0.0);
        typist.finger(2, Phase::Move, "n", 0.0);
        typist.finger(2, Phase::End, "n", 0.0);
        assert_eq!(typist.text(), "an");

        typist.finger(3, Phase::Start, "q", 0.0);
        typist.screen.touch(3, Phase::Cancel, pos2(0.0, 0.0), 0.0, &mut typist.language, &mut typist.events);
        typist.finger(4, Phase::Start, "w", 0.0);
        typist.screen.touch(4, Phase::End, pos2(10.0, 10.0), 0.0, &mut typist.language, &mut typist.events);
        assert_eq!(typist.text(), "an");
    }

    #[test]
    fn a_held_backspace_repeats() {
        let mut typist = Typist::new();
        typist.finger(0, Phase::Start, "backspace", 1.0);
        assert_eq!(typist.presses(Key::Backspace), 1);
        assert_eq!(typist.screen.tick(1.2, &mut typist.language, &mut typist.events), Some(1.0 + REPEAT_DELAY));
        let next = typist.screen.tick(1.5, &mut typist.language, &mut typist.events).unwrap();
        typist.screen.tick(next, &mut typist.language, &mut typist.events);
        assert_eq!(typist.presses(Key::Backspace), 3);
        typist.finger(0, Phase::End, "backspace", 2.0);
        assert_eq!(typist.screen.tick(3.0, &mut typist.language, &mut typist.events), None);
        assert_eq!(typist.presses(Key::Backspace), 3);
    }
}

use std::time::Duration;

use android_egui::egui::{self, Align2, Color32, ComboBox, Slider, ViewportCommand, Window};
use android_egui::{Backend, Capture, Draw, Overlay, Result, Touch};

fn main() -> Result<()> {
    let backend = std::env::args().nth(1).and_then(|name| Backend::parse(&name)).unwrap_or_default();
    let mut overlay = Overlay::new("android-egui", backend)?;
    let mut menu = Menu::new(&overlay);

    while overlay.frame(|ui| menu.show(ui))? {
        overlay.set_capture(menu.capture)?;
        overlay.set_mirroring(menu.mirroring);
        if menu.control != menu.touch.is_control() {
            menu.control = menu.touch.set_control(menu.control).is_ok() && menu.control;
        }
    }
    Ok(())
}

struct Menu {
    title: String,
    scale: f32,
    capture: Capture,
    mirroring: bool,
    control: bool,
    items: bool,
    text: String,
    touch: Touch,
}

impl Menu {
    fn new(overlay: &Overlay) -> Self {
        let title = format!("Android EGui · {}", overlay.backend());
        Self { title, scale: 1.0, capture: Capture::Visible, mirroring: false, control: false, items: true, text: String::new(), touch: overlay.touch() }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        if self.items {
            items(ui);
        }
        let context = ui.ctx().clone();
        Window::new(&self.title).default_pos([80.0, 80.0]).show(&context, |ui| {
            let scale = ui.add(Slider::new(&mut self.scale, 0.5..=3.0).text("Scale"));
            if scale.drag_stopped() || (scale.changed() && !scale.dragged()) {
                context.set_zoom_factor(self.scale);
            }
            ComboBox::from_label("Screenshots and recordings").selected_text(format!("{:?}", self.capture)).show_ui(ui, |ui| {
                for capture in [Capture::Visible, Capture::Hidden, Capture::Secure] {
                    ui.selectable_value(&mut self.capture, capture, format!("{capture:?}"));
                }
            });
            if self.capture == Capture::Visible {
                ui.checkbox(&mut self.mirroring, "Mirror to screen recorders");
            }
            ui.checkbox(&mut self.items, "Draw items");
            ui.checkbox(&mut self.control, "Control touches");
            if self.control && ui.button("Swipe up").clicked() {
                swipe(self.touch.clone(), Draw::new(ui).size());
            }
            ui.text_edit_singleline(&mut self.text);
            if ui.button("Close").clicked() {
                context.send_viewport_cmd(ViewportCommand::Close);
            }
        });
    }
}

/// Things that are not part of the menu: boxes that follow positions of their own.
fn items(ui: &egui::Ui) {
    let draw = Draw::new(ui);
    let [width, height] = draw.size();
    let time = ui.input(|input| input.time) as f32;
    for index in 0..5 {
        let turn = time * (0.3 + index as f32 * 0.07) + index as f32 * 1.3;
        let [x, y] = [width * (0.5 + 0.38 * turn.cos()), height * (0.5 + 0.3 * (turn * 1.4).sin())];
        draw.line([width / 2.0, height], [x, y + 90.0], 2.0, Color32::from_white_alpha(90));
        draw.rect([x - 45.0, y - 90.0], [x + 45.0, y + 90.0], 3.0, Color32::from_rgb(80, 220, 120));
        draw.text([x, y - 98.0], Align2::CENTER_BOTTOM, format!("item {index}"), 30.0, Color32::WHITE);
    }
    ui.ctx().request_repaint();
}

/// A finger of its own, dragged across the screen while the real ones stay free.
fn swipe(touch: Touch, [width, height]: [f32; 2]) {
    std::thread::spawn(move || touch.swipe([width * 0.75, height * 0.7], [width * 0.75, height * 0.3], Duration::from_millis(400)));
}

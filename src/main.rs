use std::time::Duration;

use android_egui::egui::{self, Align2, Color32, ComboBox, FontId, LayerId, Rect, Slider, Stroke, StrokeKind, ViewportCommand, Window, pos2};
use android_egui::{ANativeWindowCreator, Capture, Graphics, GraphicsApi, Result, Touch};

fn main() -> Result<()> {
    let api = std::env::args().nth(1).and_then(|name| GraphicsApi::parse(&name)).unwrap_or_default();
    let window = ANativeWindowCreator::create("android-egui")?;
    let mut graphics = Graphics::create(&window, api)?;
    let touch = Touch::init(&mut graphics)?;

    let title = format!("Android EGui · {}", graphics.name());
    let (mut scale, mut capture, mut mirroring) = (1.0, Capture::Visible, false);
    let (mut control, mut show_items, mut text) = (false, true, String::new());

    while graphics.frame(|ui| {
        if show_items {
            items(ui);
        }
        let context = ui.ctx().clone();
        Window::new(&title).default_pos([80.0, 80.0]).show(&context, |ui| {
            let slider = ui.add(Slider::new(&mut scale, 0.5..=3.0).text("Scale"));
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                context.set_zoom_factor(scale);
            }
            ComboBox::from_label("Screenshots and recordings").selected_text(format!("{capture:?}")).show_ui(ui, |ui| {
                for mode in [Capture::Visible, Capture::Hidden, Capture::Secure] {
                    ui.selectable_value(&mut capture, mode, format!("{mode:?}"));
                }
            });
            if capture == Capture::Visible {
                ui.checkbox(&mut mirroring, "Mirror to screen recorders");
            }
            ui.checkbox(&mut show_items, "Draw items");
            ui.checkbox(&mut control, "Control touches");
            if control && ui.button("Swipe up").clicked() {
                let display = window.display();
                swipe(touch.clone(), [display.width as f32, display.height as f32]);
            }
            ui.text_edit_singleline(&mut text);
            if ui.button("Close").clicked() {
                context.send_viewport_cmd(ViewportCommand::Close);
            }
        });
    })? {
        window.set_capture(capture)?;
        window.set_mirroring(mirroring);
        if control != touch.is_control() {
            control = touch.set_control(control).is_ok() && control;
        }
    }
    Ok(())
}

/// Things that are not part of the menu: boxes that follow positions of their own. They are
/// drawn behind the windows, on the background layer, which takes no touches. Positions are
/// kept in pixels of the screen, as touches are; egui counts in points.
fn items(ui: &egui::Ui) {
    let context = ui.ctx();
    let painter = context.layer_painter(LayerId::background());
    let pixel = 1.0 / context.pixels_per_point();
    let point = |x: f32, y: f32| pos2(x * pixel, y * pixel);
    let screen = context.viewport_rect().size() / pixel;
    let (width, height) = (screen.x, screen.y);
    let time = ui.input(|input| input.time) as f32;
    for index in 0..5 {
        let turn = time * (0.3 + index as f32 * 0.07) + index as f32 * 1.3;
        let [x, y] = [width * (0.5 + 0.38 * turn.cos()), height * (0.5 + 0.3 * (turn * 1.4).sin())];
        painter.line_segment([point(width / 2.0, height), point(x, y + 90.0)], Stroke::new(2.0 * pixel, Color32::from_white_alpha(90)));
        let outline = Stroke::new(3.0 * pixel, Color32::from_rgb(80, 220, 120));
        painter.rect_stroke(Rect::from_min_max(point(x - 45.0, y - 90.0), point(x + 45.0, y + 90.0)), 0.0, outline, StrokeKind::Middle);
        painter.text(point(x, y - 98.0), Align2::CENTER_BOTTOM, format!("item {index}"), FontId::proportional(30.0 * pixel), Color32::WHITE);
    }
    context.request_repaint();
}

/// A finger of its own, dragged across the screen while the real ones stay free.
fn swipe(touch: Touch, [width, height]: [f32; 2]) {
    std::thread::spawn(move || touch.swipe([width * 0.75, height * 0.7], [width * 0.75, height * 0.3], Duration::from_millis(400)));
}

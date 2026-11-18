pub mod texture;

use egui::{Align2, Color32, FontId, LayerId, Painter, Rect, Stroke, StrokeKind, Ui, pos2};

pub use texture::Texture;

/// Draws straight onto the screen, behind the windows, in display pixels: the space touches
/// and the display size are in. Nothing drawn here takes a touch, so whatever is underneath
/// stays usable. Call it every frame with the latest positions.
pub struct Draw {
    painter: Painter,
    points: f32,
}

impl Draw {
    pub fn new(ui: &Ui) -> Self {
        let context = ui.ctx();
        Self { painter: context.layer_painter(LayerId::background()), points: 1.0 / context.pixels_per_point() }
    }

    /// The size of the screen in pixels.
    pub fn size(&self) -> [f32; 2] {
        let screen = self.painter.ctx().viewport_rect().size() / self.points;
        [screen.x, screen.y]
    }

    pub fn line(&self, from: [f32; 2], to: [f32; 2], width: f32, color: Color32) {
        self.painter.line_segment([self.point(from), self.point(to)], self.stroke(width, color));
    }

    pub fn rect(&self, min: [f32; 2], max: [f32; 2], width: f32, color: Color32) {
        self.painter.rect_stroke(self.area(min, max), 0.0, self.stroke(width, color), StrokeKind::Middle);
    }

    pub fn rect_filled(&self, min: [f32; 2], max: [f32; 2], color: Color32) {
        self.painter.rect_filled(self.area(min, max), 0.0, color);
    }

    pub fn circle(&self, centre: [f32; 2], radius: f32, width: f32, color: Color32) {
        self.painter.circle_stroke(self.point(centre), radius * self.points, self.stroke(width, color));
    }

    pub fn circle_filled(&self, centre: [f32; 2], radius: f32, color: Color32) {
        self.painter.circle_filled(self.point(centre), radius * self.points, color);
    }

    /// Writes text `size` pixels tall, with `anchor` saying which point of it sits at `at`.
    pub fn text(&self, at: [f32; 2], anchor: Align2, text: impl ToString, size: f32, color: Color32) {
        self.painter.text(self.point(at), anchor, text, FontId::proportional(size * self.points), color);
    }

    /// Draws a picture loaded with [`crate::Overlay::load_image`] into the rectangle from `min`
    /// to `max`.
    pub fn image(&self, texture: &Texture, min: [f32; 2], max: [f32; 2]) {
        let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        self.painter.image(texture.id(), self.area(min, max), whole, Color32::WHITE);
    }

    fn point(&self, [x, y]: [f32; 2]) -> egui::Pos2 {
        pos2(x * self.points, y * self.points)
    }

    fn area(&self, min: [f32; 2], max: [f32; 2]) -> Rect {
        Rect::from_min_max(self.point(min), self.point(max))
    }

    fn stroke(&self, width: f32, color: Color32) -> Stroke {
        Stroke::new(width * self.points, color)
    }
}

#[cfg(test)]
mod tests {
    use egui::epaint::Shape;
    use egui::{Context, RawInput, ViewportId, vec2};

    use super::*;

    #[test]
    fn pixels_are_turned_into_points() {
        let context = Context::default();
        let mut input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))), ..Default::default() };
        input.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(2.0);
        let mut size = [0.0; 2];
        let mut output = context.run_ui(input, |ui| {
            let draw = Draw::new(ui);
            size = draw.size();
            draw.rect_filled([10.0, 20.0], [30.0, 60.0], Color32::RED);
            draw.line([0.0, 0.0], [100.0, 50.0], 4.0, Color32::WHITE);
        });
        output.textures_delta.clear();
        assert_eq!(size, [800.0, 600.0]);

        let shapes: Vec<&Shape> = output.shapes.iter().map(|clipped| &clipped.shape).collect();
        assert!(shapes.iter().any(|shape| matches!(shape, Shape::Rect(rect) if rect.rect == Rect::from_min_max(pos2(5.0, 10.0), pos2(15.0, 30.0)))));
        assert!(shapes.iter().any(|shape| matches!(shape, Shape::LineSegment { points, stroke } if points[1] == pos2(50.0, 25.0) && stroke.width == 2.0)));
    }

    #[test]
    fn a_picture_is_drawn_with_its_texture_where_it_is_put() {
        let context = Context::default();
        let picture = texture::load(&context, "test", [2, 2], &[255; 16], 2048).unwrap();
        let mut input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))), ..Default::default() };
        input.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(2.0);
        let mut output = context.run_ui(input, |ui| Draw::new(ui).image(&picture, [20.0, 40.0], [60.0, 80.0]));
        output.textures_delta.clear();

        let place = Rect::from_min_max(pos2(10.0, 20.0), pos2(30.0, 40.0));
        let drawn = output.shapes.iter().any(|clipped| match &clipped.shape {
            Shape::Rect(rect) => rect.rect == place && rect.brush.as_ref().is_some_and(|brush| brush.fill_texture_id == picture.id()),
            Shape::Mesh(mesh) => mesh.texture_id == picture.id() && mesh.calc_bounds() == place,
            _ => false,
        });
        assert!(drawn);
    }
}

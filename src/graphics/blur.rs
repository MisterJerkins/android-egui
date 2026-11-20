use std::cell::RefCell;
use std::rc::Rc;

use egui::{AreaState, Context, CornerRadius, LayerId, Order, Painter, Rect};

use crate::window::BlurRegion;

/// The screen behind the interface, blurred: behind every window, behind any rectangle, or both.
/// [`crate::Graphics::blur`] gives the one that belongs to the graphics. It is cheap to clone,
/// and every clone is the same blur.
///
/// The system does the blurring (Android 12 and later, on devices whose compositor is built for
/// it). Nothing is drawn for it here, and what is behind is blurred anew whenever it changes
/// with the interface standing still. It costs the system the same for any radius, any size and
/// any number of parts, more for every other radius in use at once, which is why a frame has
/// three radii at the most, and nothing while the screen behind stays as it is. Where the
/// system cannot blur, everything here does nothing.
///
/// The blurred screen lies under all that the interface draws: it shows where that is not opaque.
#[derive(Clone)]
pub struct Blur(Rc<RefCell<State>>);

struct State {
    available: bool,
    window_radius: f32,
    parts: Vec<Part>,
}

/// What is to be blurred, in the points of the layer it is drawn on.
struct Part {
    layer: LayerId,
    rect: Rect,
    radius: f32,
    /// Top left, top right, bottom left, bottom right.
    corners: [f32; 4],
}

impl Blur {
    pub(crate) fn new(available: bool) -> Self {
        Self(Rc::new(RefCell::new(State { available, window_radius: 0.0, parts: Vec::new() })))
    }

    /// Whether the system blurs at all. Nothing here needs this asked first.
    pub fn available(&self) -> bool {
        self.0.borrow().available
    }

    pub fn window_radius(&self) -> f32 {
        self.0.borrow().window_radius
    }

    /// Blurs the screen behind every window by this many points, from the frame drawn next.
    /// Zero, which is how it starts, for none. A window here is whatever egui floats above the
    /// background: a menu, a tooltip and a bare [`egui::Area`] as well.
    pub fn set_window_radius(&self, radius: f32) {
        self.0.borrow_mut().window_radius = radius.max(0.0);
    }

    /// Blurs the screen behind a rectangle by `radius` points in the frame being built, so it is
    /// called in every frame the blur is to stay. The place, the rounding and the clipping are
    /// those of `painter.rect_filled(rect, corner_radius, colour)`.
    pub fn rect(&self, painter: &Painter, rect: Rect, corner_radius: impl Into<CornerRadius>, radius: f32) {
        let state = &mut *self.0.borrow_mut();
        if !state.available {
            return;
        }
        // Cut the way the painter cuts what it draws. A corner keeps its rounding only while
        // neither of its sides was cut.
        let cut = rect.intersect(painter.clip_rect());
        let [left, top, right, bottom] = [cut.min.x == rect.min.x, cut.min.y == rect.min.y, cut.max.x == rect.max.x, cut.max.y == rect.max.y];
        let corner = corner_radius.into();
        let kept = |whole: bool, corner: u8| if whole { f32::from(corner) } else { 0.0 };
        let corners = [kept(left && top, corner.nw), kept(right && top, corner.ne), kept(left && bottom, corner.sw), kept(right && bottom, corner.se)];
        state.parts.push(Part { layer: painter.layer_id(), rect: cut, radius, corners });
    }

    /// For the graphics, at the start of a frame: the rectangles of the frame before are forgotten.
    pub(crate) fn new_frame(&self) {
        self.0.borrow_mut().parts.clear();
    }

    /// For the graphics, when the system has refused: from now on nothing is asked of it.
    pub(crate) fn give_up(&self) {
        self.0.borrow_mut().available = false;
    }

    /// For the graphics, once egui has made a frame up: the parts of the layer to blur in it,
    /// in pixels of a screen of the given size.
    pub(crate) fn collect(&self, context: &Context, pixels_per_point: f32, screen: [u32; 2]) -> Vec<BlurRegion> {
        let state = &*self.0.borrow();
        let mut regions = Vec::new();
        if !state.available {
            return regions;
        }
        // A layer may be moved as a whole, as the windows are when they go up above an
        // on-screen keyboard.
        let mut push = |part: &Part| {
            let to_screen = context.layer_transform_to_global(part.layer).unwrap_or_default();
            let (scale, rect) = (to_screen.scaling * pixels_per_point, to_screen * part.rect);
            let pixel = |point: f32, most: u32| (point * pixels_per_point).clamp(0.0, most as f32).round() as i32;
            let mut region = BlurRegion {
                radius: (part.radius.max(0.0) * scale).round() as u32,
                corners: part.corners.map(|corner| corner * scale),
                alpha: 1.0,
                left: pixel(rect.min.x, screen[0]),
                top: pixel(rect.min.y, screen[1]),
                right: pixel(rect.max.x, screen[0]),
                bottom: pixel(rect.max.y, screen[1]),
            };
            // Nothing to see of it: no radius, or no part of it on the screen. Past sixty-four
            // parts, which is as many as the system was seen to take in its stride, the rest go
            // without.
            if region.radius == 0 || region.right <= region.left || region.bottom <= region.top || regions.len() == 64 {
                return;
            }
            // The system goes over the screen once more for every radius in use, and that is
            // what its blurring costs. So a frame has three radii at the most: a fourth takes
            // the nearest.
            let radii = |regions: &[BlurRegion]| (0..regions.len()).filter(|&index| !regions[..index].iter().any(|other| other.radius == regions[index].radius)).count();
            if radii(&regions) == 3 && !regions.iter().any(|other| other.radius == region.radius) {
                region.radius = regions.iter().map(|other| other.radius).min_by_key(|other| other.abs_diff(region.radius)).unwrap_or(region.radius);
            }
            regions.push(region);
        };
        if state.window_radius > 0.0 {
            let style = context.global_style();
            let floating = context.memory(|memory| {
                let areas = memory.areas();
                areas.visible_layer_ids().into_iter().filter(|&layer| layer.order != Order::Background && areas.parent_layer(layer).is_none()).collect::<Vec<_>>()
            });
            let part = |layer: LayerId| {
                let corner = if layer.order == Order::Middle { style.visuals.window_corner_radius } else { style.visuals.menu_corner_radius };
                let corners = [corner.nw, corner.ne, corner.sw, corner.se].map(f32::from);
                Some(Part { layer, rect: AreaState::load(context, layer.id)?.rect(), radius: state.window_radius, corners })
            };
            let mut windows: Vec<Part> = floating.into_iter().filter_map(part).collect();
            // egui keeps its windows in no order, and the same parts in another order would be
            // told to the system as a change.
            windows.sort_by_key(|part| [part.rect.min.x, part.rect.min.y, part.rect.max.x, part.rect.max.y].map(|edge| edge.round() as i64));
            windows.iter().for_each(&mut push);
        }
        state.parts.iter().for_each(&mut push);
        regions
    }
}

#[cfg(test)]
mod tests {
    use egui::emath::TSTransform;
    use egui::{Area, Context, Id, LayerId, Order, RawInput, Rect, Sense, Ui, pos2, vec2};

    use super::Blur;

    /// egui without a screen, a thousand points each way, and the parts to blur in a frame of
    /// what `ui` builds. A new area takes a frame to find its size, so the frame is the third.
    fn parts(blur: &Blur, context: &Context, mut ui: impl FnMut(&mut Ui)) -> Vec<String> {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 1000.0))), ..Default::default() };
        for _ in 0..3 {
            blur.new_frame();
            context.run_ui(input.clone(), &mut ui).textures_delta.clear();
        }
        let part = |region: &crate::window::BlurRegion| {
            format!("{} px at {},{} to {},{} corners {:?}", region.radius, region.left, region.top, region.right, region.bottom, region.corners)
        };
        blur.collect(context, 1.0, [1000, 1000]).iter().map(part).collect()
    }

    /// A window of bare egui: an area three hundred points by four hundred.
    fn menu(context: &Context, inside: impl FnOnce(&mut Ui)) {
        Area::new(Id::unique("menu")).fixed_pos(pos2(100.0, 200.0)).show(context, |ui| {
            ui.allocate_exact_size(vec2(300.0, 400.0), Sense::hover());
            inside(ui);
        });
    }

    fn square(left: f32, top: f32, side: f32) -> Rect {
        Rect::from_min_size(pos2(left, top), vec2(side, side))
    }

    #[test]
    fn nothing_is_blurred_until_a_radius_is_given() {
        let (blur, context) = (Blur::new(true), Context::default());
        assert!(parts(&blur, &context, |ui| menu(ui.ctx(), |_| {})).is_empty());
        assert_eq!(blur.window_radius(), 0.0);
    }

    #[test]
    fn a_window_is_blurred_over_its_whole_rectangle_with_the_rounding_of_windows() {
        let (blur, context) = (Blur::new(true), Context::default());
        blur.set_window_radius(40.0);
        assert_eq!(parts(&blur, &context, |ui| menu(ui.ctx(), |_| {})), ["40 px at 100,200 to 400,600 corners [6.0, 6.0, 6.0, 6.0]"]);
    }

    #[test]
    fn a_rectangle_is_blurred_where_a_filled_one_would_be_drawn() {
        let (blur, context) = (Blur::new(true), Context::default());
        let place = Rect::from_min_max(pos2(10.2, 20.7), pos2(110.2, 220.7));
        let seen = parts(&blur, &context, |ui| blur.rect(&ui.ctx().layer_painter(LayerId::background()), place, 8, 25.0));
        assert_eq!(seen, ["25 px at 10,21 to 110,221 corners [8.0, 8.0, 8.0, 8.0]"]);
    }

    #[test]
    fn a_rectangle_is_cut_as_the_drawing_is_and_a_cut_corner_is_square() {
        let (blur, context) = (Blur::new(true), Context::default());
        let seen = parts(&blur, &context, |ui| {
            let painter = ui.ctx().layer_painter(LayerId::background()).with_clip_rect(square(200.0, 200.0, 100.0));
            blur.rect(&painter, square(150.0, 150.0, 100.0), 10, 30.0);
        });
        assert_eq!(seen, ["30 px at 200,200 to 250,250 corners [0.0, 0.0, 0.0, 10.0]"]);
    }

    #[test]
    fn what_is_on_a_layer_goes_where_the_layer_is_moved_and_the_rest_stays() {
        let (blur, context) = (Blur::new(true), Context::default());
        blur.set_window_radius(40.0);
        let seen = parts(&blur, &context, |ui| {
            let context = ui.ctx().clone();
            blur.rect(&context.layer_painter(LayerId::background()), square(0.0, 500.0, 100.0), 0, 40.0);
            menu(&context, |ui| blur.rect(ui.painter(), square(150.0, 300.0, 50.0), 0, 40.0));
            // What the on-screen keyboard does to make room for itself.
            context.set_transform_layer(LayerId::new(Order::Middle, Id::unique("menu")), TSTransform::from_translation(vec2(0.0, -150.0)));
        });
        assert_eq!(
            seen,
            [
                "40 px at 100,50 to 400,450 corners [6.0, 6.0, 6.0, 6.0]",
                "40 px at 0,500 to 100,600 corners [0.0, 0.0, 0.0, 0.0]",
                "40 px at 150,150 to 200,200 corners [0.0, 0.0, 0.0, 0.0]"
            ]
        );
    }

    #[test]
    fn what_is_off_the_screen_or_has_no_radius_is_not_asked_for() {
        let (blur, context) = (Blur::new(true), Context::default());
        let seen = parts(&blur, &context, |ui| {
            let painter = ui.ctx().layer_painter(LayerId::background());
            blur.rect(&painter, square(-300.0, 100.0, 200.0), 0, 30.0);
            blur.rect(&painter, square(100.0, 100.0, 100.0), 0, 0.0);
            blur.rect(&painter, square(900.0, 900.0, 300.0), 0, 30.0);
        });
        assert_eq!(seen, ["30 px at 900,900 to 1000,1000 corners [0.0, 0.0, 0.0, 0.0]"]);
    }

    #[test]
    fn a_fourth_radius_in_a_frame_takes_the_nearest_of_the_three_in_use() {
        let (blur, context) = (Blur::new(true), Context::default());
        let seen = parts(&blur, &context, |ui| {
            let painter = ui.ctx().layer_painter(LayerId::background());
            for radius in [10.0, 20.0, 30.0, 27.0, 20.0, 100.0] {
                blur.rect(&painter, square(0.0, 0.0, 50.0), 0, radius);
            }
        });
        let radii: Vec<&str> = seen.iter().map(|part| part.split(" px").next().unwrap_or_default()).collect();
        assert_eq!(radii, ["10", "20", "30", "30", "20", "30"]);
    }

    #[test]
    fn points_become_pixels_of_the_screen() {
        let (blur, context) = (Blur::new(true), Context::default());
        context.run_ui(RawInput::default(), |ui| blur.rect(&ui.ctx().layer_painter(LayerId::background()), square(10.0, 10.0, 50.0), 8, 20.0)).textures_delta.clear();
        let regions = blur.collect(&context, 2.0, [2000, 2000]);
        assert_eq!(regions.len(), 1);
        assert_eq!((regions[0].radius, regions[0].left, regions[0].top, regions[0].right, regions[0].bottom, regions[0].corners), (40, 20, 20, 120, 120, [16.0; 4]));
    }

    #[test]
    fn the_rectangles_of_a_frame_are_gone_in_the_next() {
        let (blur, context) = (Blur::new(true), Context::default());
        assert_eq!(parts(&blur, &context, |ui| blur.rect(&ui.ctx().layer_painter(LayerId::background()), square(0.0, 0.0, 50.0), 0, 30.0)).len(), 1);
        assert!(parts(&blur, &context, |_| {}).is_empty());
    }

    #[test]
    fn where_the_system_cannot_blur_nothing_is_asked_of_it() {
        let (blur, context) = (Blur::new(false), Context::default());
        blur.set_window_radius(40.0);
        let seen = parts(&blur, &context, |ui| {
            blur.rect(&ui.ctx().layer_painter(LayerId::background()), square(0.0, 0.0, 50.0), 0, 30.0);
            menu(ui.ctx(), |_| {});
        });
        assert!(!blur.available());
        assert!(seen.is_empty());
    }
}

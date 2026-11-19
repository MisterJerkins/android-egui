//! Drawing on the CPU: a rasterizer in safe Rust, and the backend that takes its picture to
//! the window.

mod raster;

#[cfg(target_os = "android")]
pub use backend::Software;

#[cfg(target_os = "android")]
mod backend {
    use std::collections::HashMap;

    use egui::TextureId;
    use egui::epaint::Primitive;

    use super::raster::{Canvas, Damage, Texture, clip_region};
    use crate::Result;
    use crate::graphics::{Backend, Frame};
    use crate::window::{ANativeWindow, Region};

    /// Draws on the CPU. The picture lives in memory of our own and only the tiles a frame
    /// changes are drawn again; each of the window's buffers is then brought up to date with
    /// just the part it has missed. Asking the system to carry the rest over instead would
    /// have it copy nearly the whole screen on every frame.
    pub struct Software {
        textures: HashMap<TextureId, Texture>,
        damage: Damage,
        picture: Vec<[u8; 4]>,
        size: [usize; 2],
        /// The window's buffers by address, and where each one is behind the picture.
        behind: Vec<(usize, Region)>,
        workers: usize,
    }

    impl Software {
        pub fn new() -> Self {
            let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
            log::info!("software backend ready, up to {workers} threads");
            Self { textures: HashMap::new(), damage: Damage::default(), picture: Vec::new(), size: [0; 2], behind: Vec::new(), workers }
        }
    }

    impl Backend for Software {
        fn name(&self) -> &'static str {
            "software"
        }

        fn max_texture_side(&self) -> usize {
            8192
        }

        fn rebind(&mut self, _window: &ANativeWindow) -> Result<()> {
            self.damage.reset();
            self.behind.clear();
            Ok(())
        }

        fn render(&mut self, frame: Frame) -> Result<()> {
            // New glyphs join the font texture in place; any other texture change repaints what shows it.
            let mut repainted = Vec::new();
            for (id, deltas) in frame.textures.set.drain() {
                let texture = self.textures.entry(id).or_default();
                deltas.iter().for_each(|delta| texture.update(delta));
                repainted.extend((id != TextureId::default() || deltas.iter().any(|delta| delta.pos.is_none())).then_some(id));
            }
            let [width, height] = frame.size.map(|side| side as usize);
            if self.size != [width, height] {
                self.size = [width, height];
                self.picture.clear();
                self.picture.resize(width * height, [0; 4]);
                self.damage.reset();
                self.behind.clear();
            }

            let scale = frame.pixels_per_point;
            let screen = Region { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            let meshes = || {
                frame.primitives.iter().filter_map(|clipped| match &clipped.primitive {
                    Primitive::Mesh(mesh) => Some((mesh, clip_region(clipped.clip_rect, scale).intersect(screen))),
                    Primitive::Callback(_) => None,
                })
            };
            let damage = self.damage.update(screen, scale, |id| repainted.contains(&id), meshes());
            if !damage.is_empty() {
                let textures = &self.textures;
                let draw = |mut canvas: Canvas| {
                    canvas.clear();
                    for (mesh, clip) in meshes() {
                        if let Some(texture) = textures.get(&mesh.texture_id) {
                            canvas.mesh(mesh, clip, scale, texture);
                        }
                    }
                };
                let canvas = Canvas::new(&mut self.picture, width, damage);
                // From 200 thousand pixels on, a redraw is shared between threads.
                if damage.area() < 200_000 || self.workers < 2 {
                    draw(canvas);
                } else {
                    std::thread::scope(|scope| canvas.bands(self.workers).into_iter().for_each(|band| drop(scope.spawn(|| draw(band)))));
                }
                self.present(frame.window, screen, damage)?;
            }
            for id in frame.textures.free.drain() {
                self.textures.remove(&id);
            }
            Ok(())
        }
    }

    impl Software {
        fn present(&mut self, window: &ANativeWindow, screen: Region, damage: Region) -> Result<()> {
            let mut buffer = window.lock(None)?;
            let bounds = Region { left: 0, top: 0, right: buffer.width() as i32, bottom: buffer.height() as i32 };
            let (pixels, stride) = buffer.pixels();
            let address = pixels.as_ptr() as usize;
            let known = self.behind.iter().position(|(buffer, _)| *buffer == address);
            let missing = known.map_or(screen, |index| self.behind[index].1.union(damage)).intersect(bounds);

            let (left, right) = (missing.left as usize, missing.right as usize);
            for row in missing.top as usize..missing.bottom as usize {
                pixels[row * stride + left..row * stride + right].copy_from_slice(&self.picture[row * self.size[0] + left..row * self.size[0] + right]);
            }

            self.behind.iter_mut().for_each(|(_, behind)| *behind = behind.union(damage));
            match known {
                Some(index) => self.behind[index].1 = Region::default(),
                None => {
                    // A window has fewer than eight buffers: a list that long holds addresses of buffers that are gone.
                    if self.behind.len() == 8 {
                        self.behind.clear();
                    }
                    self.behind.push((address, Region::default()));
                }
            }
            Ok(())
        }
    }
}

use egui::epaint::{ImageDelta, Mesh, Vertex};
use egui::{ImageData, Rect, TextureFilter, TextureId, TextureWrapMode};

use crate::graphics::texture_key;
use crate::utils::{HASH_SEED, mix};
use crate::window::Region;

/// The column or row whose centre is the first one at or past `edge`: the pixels a shape covers
/// are those whose centres lie inside it, as on a GPU.
fn snap(edge: f32) -> i32 {
    (edge - 0.5).ceil() as i32
}

#[inline(always)]
fn div255(value: u32) -> u32 {
    let value = value + 128;
    (value + (value >> 8)) >> 8
}

#[inline(always)]
fn modulate(a: [u8; 4], b: [u8; 4]) -> [u8; 4] {
    [0, 1, 2, 3].map(|channel| div255(u32::from(a[channel]) * u32::from(b[channel])) as u8)
}

/// Lays a premultiplied colour over another.
#[inline(always)]
fn blend(target: &mut [u8; 4], source: [u8; 4]) {
    let keep = 255 - u32::from(source[3]);
    for channel in 0..4 {
        target[channel] = (u32::from(source[channel]) + div255(u32::from(target[channel]) * keep)).min(255) as u8;
    }
}

fn fill(row: &mut [[u8; 4]], color: [u8; 4]) {
    match color {
        [_, _, _, 255] => row.fill(color),
        [0, 0, 0, 0] => {}
        _ => row.iter_mut().for_each(|pixel| blend(pixel, color)),
    }
}

pub fn clip_region(clip: Rect, scale: f32) -> Region {
    let pixel = |value: f32| (value * scale).round() as i32;
    Region { left: pixel(clip.min.x), top: pixel(clip.min.y), right: pixel(clip.max.x), bottom: pixel(clip.max.y) }
}

const TILE: i32 = 32;

/// Remembers what every tile of the screen showed, to find what a new frame changes. A tile
/// is known by a hash of the triangles that reach into it, in drawing order.
#[derive(Default)]
pub struct Damage {
    columns: i32,
    shown: Vec<u64>,
    next: Vec<u64>,
    corners: Vec<u64>,
    frame: u64,
}

impl Damage {
    /// Forgets the picture, so that the next frame counts as new all over.
    pub fn reset(&mut self) {
        self.shown.clear();
    }

    /// Takes in a frame and returns the smallest region holding every tile it changes.
    /// Meshes for which `repainted` holds count as changed even where their shape is the same.
    pub fn update<'a>(&mut self, screen: Region, scale: f32, repainted: impl Fn(TextureId) -> bool, meshes: impl Iterator<Item = (&'a Mesh, Region)>) -> Region {
        let tiles = |pixels: i32| (pixels + TILE - 1) / TILE;
        let (columns, rows) = (tiles(screen.right), tiles(screen.bottom));
        self.frame += 1;
        self.next.clear();
        self.next.resize((columns * rows).max(0) as usize, 0);

        for (mesh, clip) in meshes {
            let salt =
                [texture_key(mesh.texture_id), clip.left as u64, clip.top as u64, clip.right as u64, clip.bottom as u64, if repainted(mesh.texture_id) { self.frame } else { 0 }];
            let salt = salt.into_iter().fold(HASH_SEED, mix);
            let words: &[[u32; size_of::<Vertex>() / 4]] = bytemuck::cast_slice(&mesh.vertices);
            self.corners.clear();
            self.corners.extend(words.iter().map(|vertex| vertex.iter().fold(salt, |hash, &word| mix(hash, word.into()))));

            for triangle in mesh.indices.as_chunks::<3>().0 {
                let corner = |index: u32| mesh.vertices.get(index as usize).map(|vertex| (vertex.pos.x * scale, vertex.pos.y * scale));
                let (Some(a), Some(b), Some(c)) = (corner(triangle[0]), corner(triangle[1]), corner(triangle[2])) else { continue };
                let (left, right) = (a.0.min(b.0).min(c.0), a.0.max(b.0).max(c.0));
                let (top, bottom) = (a.1.min(b.1).min(c.1), a.1.max(b.1).max(c.1));
                let reach =
                    Region { left: left.floor() as i32, top: top.floor() as i32, right: right.ceil() as i32, bottom: bottom.ceil() as i32 }.intersect(clip).intersect(screen);
                if reach.is_empty() {
                    continue;
                }
                let hash = triangle.iter().fold(0, |hash, &index| mix(hash, self.corners[index as usize]));
                for row in reach.top / TILE..=(reach.bottom - 1) / TILE {
                    for column in reach.left / TILE..=(reach.right - 1) / TILE {
                        let tile = &mut self.next[(row * columns + column) as usize];
                        *tile = mix(*tile, hash);
                    }
                }
            }
        }

        let mut changed = Region::default();
        if columns != self.columns || self.shown.len() != self.next.len() {
            changed = screen;
        } else {
            for (index, _) in self.next.iter().zip(&self.shown).enumerate().filter(|(_, (next, shown))| next != shown) {
                let (column, row) = (index as i32 % columns, index as i32 / columns);
                changed = changed.union(Region { left: column * TILE, top: row * TILE, right: (column + 1) * TILE, bottom: (row + 1) * TILE });
            }
        }
        std::mem::swap(&mut self.shown, &mut self.next);
        self.columns = columns;
        changed.intersect(screen)
    }
}

#[derive(Default)]
pub struct Texture {
    width: usize,
    height: usize,
    pixels: Vec<[u8; 4]>,
    smooth: bool,
    repeat: bool,
}

impl Texture {
    pub fn update(&mut self, delta: &ImageDelta) {
        let ImageData::Color(image) = &delta.image;
        let [width, height] = image.size;
        let source: &[[u8; 4]] = bytemuck::cast_slice(&image.pixels);
        let Some([left, top]) = delta.pos else {
            let (smooth, repeat) = (delta.options.magnification == TextureFilter::Linear, delta.options.wrap_mode != TextureWrapMode::ClampToEdge);
            *self = Self { width, height, pixels: source.to_vec(), smooth, repeat };
            return;
        };
        if width == 0 || left + width > self.width {
            return;
        }
        for (row, line) in source.chunks_exact(width).enumerate() {
            let start = (top + row) * self.width + left;
            if let Some(target) = self.pixels.get_mut(start..start + width) {
                target.copy_from_slice(line);
            }
        }
    }

    #[inline(always)]
    fn texel(&self, x: i32, y: i32) -> [u8; 4] {
        let wrap = |value: i32, size: usize| if self.repeat { value.rem_euclid(size as i32) } else { value.clamp(0, size as i32 - 1) };
        self.pixels[wrap(y, self.height) as usize * self.width + wrap(x, self.width) as usize]
    }

    /// The colour at a position measured in texels.
    #[inline(always)]
    fn sample(&self, u: f32, v: f32) -> [u8; 4] {
        if !self.smooth {
            return self.texel(u.floor() as i32, v.floor() as i32);
        }
        let (x, y) = (u - 0.5, v - 0.5);
        let (left, top) = (x.floor(), y.floor());
        let (across, down) = (((x - left) * 256.0) as u32, ((y - top) * 256.0) as u32);
        let (left, top) = (left as i32, top as i32);
        let [a, b, c, d] = [self.texel(left, top), self.texel(left + 1, top), self.texel(left, top + 1), self.texel(left + 1, top + 1)];
        [0, 1, 2, 3].map(|channel| {
            let upper = u32::from(a[channel]) * (256 - across) + u32::from(b[channel]) * across;
            let lower = u32::from(c[channel]) * (256 - across) + u32::from(d[channel]) * across;
            ((upper * (256 - down) + lower * down + (1 << 15)) >> 16) as u8
        })
    }

    /// A run of texels starting at a texel position, when all of it lies inside the texture.
    fn run(&self, u: f32, v: f32, length: usize) -> Option<&[[u8; 4]]> {
        let inside = u >= 0.0 && v >= 0.0 && (v as usize) < self.height && u as usize + length <= self.width;
        let start = v as usize * self.width + u as usize;
        inside.then(|| &self.pixels[start..start + length])
    }
}

/// A mesh vertex in pixels and texels.
#[derive(Clone, Copy)]
struct Corner {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
    color: [u8; 4],
}

/// Rows of premultiplied RGBA to draw on, and the part of them that may change.
pub struct Canvas<'a> {
    pixels: &'a mut [[u8; 4]],
    stride: usize,
    top: i32,
    area: Region,
}

impl<'a> Canvas<'a> {
    /// `pixels` holds rows of `stride` pixels from the top of the surface; `area` must lie inside them.
    pub fn new(pixels: &'a mut [[u8; 4]], stride: usize, area: Region) -> Self {
        let area = if area.is_empty() { Region::default() } else { area };
        let first = (area.top.max(0) as usize * stride).min(pixels.len());
        Self { pixels: &mut pixels[first..], stride, top: area.top.max(0), area }
    }

    /// Cuts the canvas into horizontal bands that can be drawn at the same time.
    pub fn bands(self, count: usize) -> Vec<Self> {
        let rows = (self.area.bottom - self.area.top).max(0) as usize;
        let height = rows.div_ceil(count.max(1)).max(1);
        let (mut bands, mut rest, mut top) = (Vec::new(), self.pixels, self.area.top);
        while top < self.area.bottom {
            let bottom = (top + height as i32).min(self.area.bottom);
            let (band, tail) = rest.split_at_mut(((bottom - top) as usize * self.stride).min(rest.len()));
            bands.push(Self { pixels: band, stride: self.stride, top, area: Region { top, bottom, ..self.area } });
            (rest, top) = (tail, bottom);
        }
        bands
    }

    pub fn clear(&mut self) {
        for y in self.area.top..self.area.bottom {
            self.row(y, self.area.left, self.area.right).fill([0; 4]);
        }
    }

    pub fn mesh(&mut self, mesh: &Mesh, clip: Region, scale: f32, texture: &Texture) {
        let clip = clip.intersect(self.area);
        if clip.is_empty() || texture.pixels.is_empty() {
            return;
        }
        let size = [texture.width as f32, texture.height as f32];
        let corner = |index: u32| {
            let vertex = mesh.vertices.get(index as usize)?;
            Some(Corner { x: vertex.pos.x * scale, y: vertex.pos.y * scale, u: vertex.uv.x * size[0], v: vertex.uv.y * size[1], color: vertex.color.to_array() })
        };
        let mut indices = mesh.indices.as_slice();
        while let [first, second, third, rest @ ..] = indices {
            let (Some(a), Some(b), Some(c)) = (corner(*first), corner(*second), corner(*third)) else { return };
            // Rectangles come as two triangles over four corners; those need no edge walking.
            if let [fourth, fifth, sixth, after @ ..] = rest
                && (fourth, fifth) == (third, second)
                && let Some(d) = corner(*sixth)
                && self.rectangle([a, b, c, d], clip, texture)
            {
                indices = after;
                continue;
            }
            self.triangle([a, b, c], clip, texture);
            indices = rest;
        }
    }

    fn row(&mut self, y: i32, left: i32, right: i32) -> &mut [[u8; 4]] {
        let start = (y - self.top) as usize * self.stride;
        &mut self.pixels[start + left as usize..start + right as usize]
    }

    /// Draws an upright rectangle given as top-left, top-right, bottom-left and bottom-right
    /// corners. Returns `false` when the corners are anything else.
    fn rectangle(&mut self, [a, b, c, d]: [Corner; 4], clip: Region, texture: &Texture) -> bool {
        let upright = a.y == b.y && c.y == d.y && a.x == c.x && b.x == d.x && a.x < b.x && a.y < c.y;
        let mapped = a.v == b.v && c.v == d.v && a.u == c.u && b.u == d.u;
        if !(upright && mapped && a.color == b.color && b.color == c.color && c.color == d.color) {
            return false;
        }
        let area = Region { left: snap(a.x), top: snap(a.y), right: snap(d.x), bottom: snap(d.y) }.intersect(clip);
        if area.is_empty() {
            return true;
        }
        let (across, down) = ((d.u - a.u) / (d.x - a.x), (d.v - a.v) / (d.y - a.y));
        let whole = (across - 1.0).abs() < 1e-3 && (down - 1.0).abs() < 1e-3;
        let plain = (across == 0.0 && down == 0.0).then(|| modulate(texture.sample(a.u, a.v), a.color));
        for y in area.top..area.bottom {
            let (u, v) = (a.u + (area.left as f32 + 0.5 - a.x) * across, a.v + (y as f32 + 0.5 - a.y) * down);
            let row = self.row(y, area.left, area.right);
            if let Some(color) = plain {
                fill(row, color);
            } else if let Some(texels) = texture.run(u, v, row.len()).filter(|_| whole) {
                row.iter_mut().zip(texels).for_each(|(pixel, texel)| blend(pixel, modulate(*texel, a.color)));
            } else {
                row.iter_mut().enumerate().for_each(|(index, pixel)| blend(pixel, modulate(texture.sample(u + index as f32 * across, v), a.color)));
            }
        }
        true
    }

    fn triangle(&mut self, [a, b, c]: [Corner; 3], clip: Region, texture: &Texture) {
        let top = snap(a.y.min(b.y).min(c.y)).max(clip.top);
        let bottom = snap(a.y.max(b.y).max(c.y)).min(clip.bottom);
        let (abx, aby, acx, acy) = (b.x - a.x, b.y - a.y, c.x - a.x, c.y - a.y);
        let area = abx * acy - aby * acx;
        if top >= bottom || area == 0.0 || !area.is_finite() {
            return;
        }
        // How fast a value given at the three corners changes per pixel across and down.
        let slope = |at_a: f32, at_b: f32, at_c: f32| (((at_b - at_a) * acy - (at_c - at_a) * aby) / area, ((at_c - at_a) * abx - (at_b - at_a) * acx) / area);
        let channel = |index: usize| slope(f32::from(a.color[index]), f32::from(b.color[index]), f32::from(c.color[index]));
        let shades = [channel(0), channel(1), channel(2), channel(3)];
        let (along_u, along_v) = (slope(a.u, b.u, c.u), slope(a.v, b.v, c.v));
        let plain = a.color == b.color && b.color == c.color;
        let flat = a.u == b.u && b.u == c.u && a.v == b.v && b.v == c.v;
        let texel = texture.sample(a.u, a.v);

        for y in top..bottom {
            let centre = y as f32 + 0.5;
            let (mut left, mut right) = (f32::INFINITY, f32::NEG_INFINITY);
            for (from, to) in [(a, b), (b, c), (c, a)] {
                // Always walked from its upper end, so both triangles on an edge agree on where it runs.
                let (from, to) = if (from.y, from.x) <= (to.y, to.x) { (from, to) } else { (to, from) };
                if centre >= from.y && centre < to.y {
                    let x = from.x + (centre - from.y) * (to.x - from.x) / (to.y - from.y);
                    (left, right) = (left.min(x), right.max(x));
                }
            }
            let (start, end) = (snap(left).max(clip.left), snap(right).min(clip.right));
            if start >= end {
                continue;
            }
            let (dx, dy) = (start as f32 + 0.5 - a.x, centre - a.y);
            let row = self.row(y, start, end);
            if plain && flat {
                fill(row, modulate(texel, a.color));
                continue;
            }
            let mut shade = [0, 1, 2, 3].map(|index| f32::from(a.color[index]) + dx * shades[index].0 + dy * shades[index].1 + 0.5);
            let (mut u, mut v) = (a.u + dx * along_u.0 + dy * along_u.1, a.v + dx * along_v.0 + dy * along_v.1);
            for pixel in row {
                let color = if plain { a.color } else { shade.map(|value| value as u8) };
                let source = if flat { texel } else { texture.sample(u, v) };
                blend(pixel, if source == [255; 4] { color } else { modulate(source, color) });
                shade = [0, 1, 2, 3].map(|index| shade[index] + shades[index].0);
                (u, v) = (u + along_u.0, v + along_v.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use egui::epaint::{ColorImage, Vertex};
    use egui::{Color32, TextureId, TextureOptions, pos2, vec2};

    use super::*;

    const SIDE: usize = 16;
    const ALL: Region = Region { left: 0, top: 0, right: SIDE as i32, bottom: SIDE as i32 };

    fn texture(width: usize, height: usize, pixel: impl Fn(usize, usize) -> [u8; 4], options: TextureOptions) -> Texture {
        let pixels = (0..width * height).map(|index| pixel(index % width, index / width)).map(|[r, g, b, a]| Color32::from_rgba_premultiplied(r, g, b, a)).collect();
        let mut texture = Texture::default();
        texture.update(&ImageDelta::full(ColorImage::new([width, height], pixels), options));
        texture
    }

    fn white() -> Texture {
        texture(1, 1, |_, _| [255; 4], TextureOptions::LINEAR)
    }

    fn mesh(vertices: &[([f32; 2], [f32; 2], [u8; 4])], indices: &[u32]) -> Mesh {
        let vertices =
            vertices.iter().map(|&([x, y], [u, v], [r, g, b, a])| Vertex { pos: pos2(x, y), uv: pos2(u, v), color: Color32::from_rgba_premultiplied(r, g, b, a) }).collect();
        Mesh { indices: indices.to_vec(), vertices, texture_id: TextureId::default() }
    }

    fn rectangle(left: f32, top: f32, right: f32, bottom: f32, color: [u8; 4]) -> Mesh {
        mesh(&[([left, top], [0.0; 2], color), ([right, top], [0.0; 2], color), ([left, bottom], [0.0; 2], color), ([right, bottom], [0.0; 2], color)], &[0, 1, 2, 2, 1, 3])
    }

    fn draw(meshes: &[&Mesh], texture: &Texture, clip: Region, background: [u8; 4]) -> Vec<[u8; 4]> {
        let mut pixels = vec![background; SIDE * SIDE];
        let mut canvas = Canvas::new(&mut pixels, SIDE, ALL);
        meshes.iter().for_each(|mesh| canvas.mesh(mesh, clip, 1.0, texture));
        pixels
    }

    fn covered(pixels: &[[u8; 4]], background: [u8; 4]) -> Vec<(usize, usize)> {
        (0..SIDE * SIDE).filter(|&index| pixels[index] != background).map(|index| (index % SIDE, index / SIDE)).collect()
    }

    #[test]
    fn dividing_by_255_rounds_like_the_exact_division() {
        for value in 0..=255 * 255u32 {
            assert_eq!(div255(value), (value as f32 / 255.0).round() as u32, "{value}");
        }
        let mut pixel = [100, 50, 0, 200];
        blend(&mut pixel, [64, 0, 0, 128]);
        assert_eq!(pixel, [114, 25, 0, 228]);
    }

    #[test]
    fn a_rectangle_covers_the_pixels_whose_centres_it_contains() {
        let pixels = draw(&[&rectangle(2.4, 3.6, 5.5, 6.5, [10, 20, 30, 255])], &white(), ALL, [0; 4]);
        let expected: Vec<(usize, usize)> = (4..6).flat_map(|y| (2..5).map(move |x| (x, y))).collect();
        let mut seen = covered(&pixels, [0; 4]);
        seen.sort_by_key(|&(x, y)| (y, x));
        assert_eq!(seen, expected);
        assert_eq!(pixels[4 * SIDE + 2], [10, 20, 30, 255]);
    }

    #[test]
    fn triangles_sharing_an_edge_leave_no_seam_and_no_overlap() {
        let (red, also_red) = ([100, 0, 0, 100], [101, 0, 0, 100]);
        let corners = [([1.3, 1.7], [0.0; 2], red), ([13.2, 2.1], [0.0; 2], red), ([2.2, 12.6], [0.0; 2], red), ([14.9, 14.4], [0.0; 2], also_red)];
        let pixels = draw(&[&mesh(&corners, &[0, 1, 2, 2, 1, 3])], &white(), ALL, [0, 0, 0, 255]);
        let touched = covered(&pixels, [0, 0, 0, 255]);
        assert!(touched.len() > 100);
        for (x, y) in touched {
            assert!((100..=101).contains(&pixels[y * SIDE + x][0]), "pixel {x},{y} was blended {:?}", pixels[y * SIDE + x]);
        }
        let fan: Vec<_> = [[8.0, 8.0], [1.0, 1.0], [15.0, 1.0], [15.0, 15.0], [1.0, 15.0]].iter().map(|&position| (position, [0.0; 2], red)).collect();
        let pixels = draw(&[&mesh(&fan, &[0, 1, 2, 0, 2, 3, 0, 3, 4, 0, 4, 1])], &white(), ALL, [0, 0, 0, 255]);
        assert_eq!(covered(&pixels, [0, 0, 0, 255]).len(), 14 * 14);
        assert!(pixels.iter().all(|pixel| pixel[0] == 0 || pixel[0] == 100));
    }

    #[test]
    fn colours_shade_smoothly_across_a_triangle() {
        let corners = [([0.0, 0.0], [0.0; 2], [0, 0, 0, 255]), ([16.0, 0.0], [0.0; 2], [160, 0, 0, 255]), ([0.0, 16.0], [0.0; 2], [0, 160, 0, 255])];
        let pixels = draw(&[&mesh(&corners, &[0, 1, 2])], &white(), ALL, [0; 4]);
        assert_eq!(pixels[0], [5, 5, 0, 255]);
        assert_eq!(pixels[3 * SIDE + 8], [85, 35, 0, 255]);
        assert_eq!(pixels[15 * SIDE + 15], [0; 4]);
    }

    #[test]
    fn glyphs_are_copied_texel_for_texel_and_tinted() {
        let atlas = texture(8, 8, |x, y| [(x * 30) as u8, (y * 30) as u8, 0, 255], TextureOptions::LINEAR);
        let uv = |x: f32, y: f32| [x / 8.0, y / 8.0];
        let quad = |tint| {
            mesh(&[([3.0, 5.0], uv(2.0, 1.0), tint), ([7.0, 5.0], uv(6.0, 1.0), tint), ([3.0, 8.0], uv(2.0, 4.0), tint), ([7.0, 8.0], uv(6.0, 4.0), tint)], &[0, 1, 2, 2, 1, 3])
        };
        let pixels = draw(&[&quad([255; 4])], &atlas, ALL, [0; 4]);
        assert_eq!(covered(&pixels, [0; 4]).len(), 4 * 3);
        assert_eq!(pixels[5 * SIDE + 3], [60, 30, 0, 255]);
        assert_eq!(pixels[7 * SIDE + 6], [150, 90, 0, 255]);

        let pixels = draw(&[&quad([128, 128, 128, 128])], &atlas, ALL, [0; 4]);
        assert_eq!(pixels[7 * SIDE + 6], [75, 45, 0, 128]);
    }

    #[test]
    fn stretched_textures_are_filtered_or_not_as_asked() {
        let checker = |options| texture(2, 1, |x, _| if x == 0 { [0, 0, 0, 255] } else { [200, 200, 200, 255] }, options);
        let stretched = mesh(
            &[([0.0, 0.0], [0.0, 0.0], [255; 4]), ([16.0, 0.0], [1.0, 0.0], [255; 4]), ([0.0, 4.0], [0.0, 1.0], [255; 4]), ([16.0, 4.0], [1.0, 1.0], [255; 4])],
            &[0, 1, 2, 2, 1, 3],
        );
        let smooth = draw(&[&stretched], &checker(TextureOptions::LINEAR), ALL, [0; 4]);
        assert!(smooth[0][0] == 0 && smooth[15][0] == 200 && (105..=120).contains(&smooth[8][0]), "{:?}", &smooth[..16]);
        let sharp = draw(&[&stretched], &checker(TextureOptions::NEAREST), ALL, [0; 4]);
        assert_eq!((sharp[7][0], sharp[8][0]), (0, 200));
    }

    #[test]
    fn nothing_is_drawn_outside_the_clip_or_the_canvas_area() {
        let everything = rectangle(-50.0, -50.0, 50.0, 50.0, [9, 9, 9, 255]);
        let clip = Region { left: 4, top: 5, right: 9, bottom: 7 };
        let pixels = draw(&[&everything], &white(), clip, [0; 4]);
        assert_eq!(covered(&pixels, [0; 4]).len(), 5 * 2);
        let beside = rectangle(10.0, 0.0, 14.0, 16.0, [9, 9, 9, 255]);
        assert!(covered(&draw(&[&beside], &white(), clip, [0; 4]), [0; 4]).is_empty());

        let mut pixels = vec![[1, 1, 1, 255]; SIDE * SIDE];
        let area = Region { left: 2, top: 3, right: 10, bottom: 12 };
        let bands = Canvas::new(&mut pixels, SIDE, area).bands(4);
        assert_eq!(bands.iter().map(|band| band.area.bottom - band.area.top).sum::<i32>(), 9);
        for mut band in bands {
            band.clear();
            band.mesh(&everything, ALL, 1.0, &white());
        }
        assert_eq!(covered(&pixels, [1, 1, 1, 255]).len(), 8 * 9);
        assert_eq!((pixels[3 * SIDE + 2], pixels[2 * SIDE + 2], pixels[12 * SIDE + 9]), ([9, 9, 9, 255], [1, 1, 1, 255], [1, 1, 1, 255]));
    }

    #[test]
    fn damage_is_where_the_picture_changes() {
        let screen = Region { left: 0, top: 0, right: 200, bottom: 100 };
        let (fixed, moving) = (rectangle(150.0, 60.0, 190.0, 90.0, [255; 4]), |x: f32| rectangle(x, 10.0, x + 8.0, 18.0, [255; 4]));
        let mut damage = Damage::default();
        let frame = |damage: &mut Damage, moving: &Mesh, repainted: bool| damage.update(screen, 1.0, |_| repainted, [(&fixed, screen), (moving, screen)].into_iter());

        assert_eq!(frame(&mut damage, &moving(4.0), false), screen);
        assert!(frame(&mut damage, &moving(4.0), false).is_empty());
        assert_eq!(frame(&mut damage, &moving(40.0), false), Region { left: 0, top: 0, right: 64, bottom: 32 });
        assert_eq!(frame(&mut damage, &moving(41.0), false), Region { left: 32, top: 0, right: 64, bottom: 32 });
        assert_eq!(frame(&mut damage, &moving(41.0), true), Region { left: 32, top: 0, right: 192, bottom: 96 });
        assert_eq!(damage.update(screen, 1.0, |_| false, [(&fixed, screen)].into_iter()), Region { left: 32, top: 0, right: 192, bottom: 96 });
        assert_eq!(damage.update(screen, 1.0, |_| false, [(&fixed, screen)].into_iter()), Region::default());

        damage.reset();
        assert_eq!(damage.update(screen, 1.0, |_| false, [(&fixed, screen)].into_iter()), screen);
        let clipped = Region { left: 0, top: 0, right: 160, bottom: 100 };
        assert_eq!(damage.update(screen, 1.0, |_| false, [(&fixed, clipped)].into_iter()), Region { left: 128, top: 32, right: 192, bottom: 96 });
    }

    #[test]
    fn a_patch_updates_part_of_a_texture() {
        let mut atlas = texture(4, 4, |_, _| [0, 0, 0, 255], TextureOptions::NEAREST);
        let patch = ColorImage::new([2, 2], vec![Color32::WHITE; 4]);
        atlas.update(&ImageDelta::partial([1, 2], patch, TextureOptions::NEAREST));
        assert_eq!((atlas.texel(1, 2), atlas.texel(2, 3), atlas.texel(0, 2), atlas.texel(3, 3)), ([255; 4], [255; 4], [0, 0, 0, 255], [0, 0, 0, 255]));
        assert_eq!(clip_region(Rect::from_min_size(pos2(1.0, 1.0), vec2(2.0, 3.0)), 2.25), Region { left: 2, top: 2, right: 7, bottom: 9 });
    }
}

use std::path::Path;

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use crate::{Result, bail, err};

/// A picture loaded for drawing: `ui.image(&texture)` puts it into a window, [`crate::Draw::image`]
/// anywhere on the screen, and `texture.size()` is its size in pixels. The graphics backend
/// keeps it until the last copy of the handle is dropped.
pub type Texture = TextureHandle;

/// Hands RGBA pixels, four bytes each, row after row from the top, to the graphics backend.
/// `limit` is the longest side a texture may have.
pub fn load(context: &Context, name: &str, size: [usize; 2], rgba: &[u8], limit: usize) -> Result<Texture> {
    let [width, height] = size;
    if width == 0 || height == 0 || rgba.len() != width * height * 4 {
        bail!("the picture {name} is said to be {width}x{height} and has {} bytes", rgba.len());
    }
    if width.max(height) > limit {
        bail!("the picture {name} is {width}x{height}, and a texture here may be {limit} pixels a side");
    }
    Ok(context.load_texture(name, ColorImage::from_rgba_unmultiplied(size, rgba), TextureOptions::LINEAR))
}

/// Reads a PNG or JPEG picture into its size and RGBA pixels.
pub fn decode(name: &str, data: &[u8]) -> Result<([usize; 2], Vec<u8>)> {
    let picture = image::load_from_memory(data).map_err(|error| err!("cannot read the picture {name}: {error}"))?.into_rgba8();
    Ok(([picture.width() as usize, picture.height() as usize], picture.into_raw()))
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| err!("cannot read {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ImageFormat, Rgba, RgbaImage};

    use super::*;

    fn encoded(format: ImageFormat, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let picture = RgbaImage::from_fn(16, 8, |x, y| Rgba(pixel(x, y)));
        let mut data = Cursor::new(Vec::new());
        if format == ImageFormat::Jpeg {
            image::DynamicImage::ImageRgba8(picture).into_rgb8().write_to(&mut data, format).unwrap();
        } else {
            picture.write_to(&mut data, format).unwrap();
        }
        data.into_inner()
    }

    #[test]
    fn a_png_comes_back_pixel_for_pixel_with_its_transparency() {
        let pixel = |x: u32, y: u32| [(x * 16) as u8, (y * 30) as u8, 200, if x < 8 { 255 } else { 90 }];
        let (size, rgba) = decode("test", &encoded(ImageFormat::Png, pixel)).unwrap();
        assert_eq!(size, [16, 8]);
        for (index, found) in rgba.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(*found, pixel(index as u32 % 16, index as u32 / 16));
        }
    }

    #[test]
    fn a_jpeg_comes_back_close_to_its_colours_and_opaque() {
        let (size, rgba) = decode("test", &encoded(ImageFormat::Jpeg, |_, _| [200, 120, 40, 255])).unwrap();
        assert_eq!(size, [16, 8]);
        for found in rgba.as_chunks::<4>().0 {
            assert!(found.iter().zip([200u8, 120, 40, 255]).all(|(&found, wanted)| found.abs_diff(wanted) <= 4), "pixel {found:?}");
        }
    }

    #[test]
    fn what_is_not_a_picture_is_an_error() {
        assert!(decode("test", b"not a picture").is_err());
    }

    #[test]
    fn pixels_become_a_texture_of_their_size() {
        let context = Context::default();
        let texture = load(&context, "test", [3, 2], &[255; 24], 2048).unwrap();
        assert_eq!(texture.size(), [3, 2]);
        assert!(load(&context, "short", [3, 2], &[255; 20], 2048).is_err());
        assert!(load(&context, "empty", [0, 2], &[], 2048).is_err());
        assert!(load(&context, "huge", [3, 2], &[255; 24], 2).is_err());
    }
}

//! Draw one square RGBA image at a requested size.
//!
//! SVG is rasterized at that size. A raster source is fitted once, with a
//! high-quality downscale or a single upscale. Nothing here scales an
//! already-scaled image again.

use image::RgbaImage;
use image::imageops::FilterType;

use crate::error::IconError;
use crate::source::Normalized;

pub(crate) fn square(
    source: &Normalized,
    size: u32,
    padding: f32,
    label: &str,
) -> Result<RgbaImage, IconError> {
    match source {
        Normalized::Svg(tree) => render_svg(tree, size, padding, label),
        Normalized::Raster(image) => Ok(render_raster(image, size, padding)),
    }
}

fn render_svg(
    tree: &usvg::Tree,
    size: u32,
    padding: f32,
    label: &str,
) -> Result<RgbaImage, IconError> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or_else(|| IconError::Encode {
        kind: "raster",
        detail: format!("{size}px is too large to allocate"),
    })?;
    let svg = tree.size();
    let canvas = size as f32;
    let avail = (canvas * (1.0 - 2.0 * padding)).max(1.0);
    let scale = (avail / svg.width()).min(avail / svg.height());
    let drawn_w = svg.width() * scale;
    let drawn_h = svg.height() * scale;
    let transform = tiny_skia::Transform::from_scale(scale, scale)
        .post_translate((canvas - drawn_w) / 2.0, (canvas - drawn_h) / 2.0);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    RgbaImage::from_raw(size, size, pixmap.take()).ok_or_else(|| IconError::Encode {
        kind: "raster",
        detail: format!("icon `{label}` did not produce a {size}px image"),
    })
}

fn render_raster(source: &RgbaImage, size: u32, padding: f32) -> RgbaImage {
    let canvas = size as f32;
    let avail = (canvas * (1.0 - 2.0 * padding)).max(1.0);
    let scale = (avail / source.width() as f32).min(avail / source.height() as f32);
    let width = ((source.width() as f32) * scale).round().clamp(1.0, canvas) as u32;
    let height = ((source.height() as f32) * scale)
        .round()
        .clamp(1.0, canvas) as u32;
    let fitted = if width == source.width() && height == source.height() {
        source.clone()
    } else {
        image::imageops::resize(source, width, height, FilterType::Lanczos3)
    };
    if padding == 0.0 && width == size && height == size {
        return fitted;
    }
    let mut canvas = RgbaImage::new(size, size);
    let x = (size - width) / 2;
    let y = (size - height) / 2;
    image::imageops::overlay(&mut canvas, &fitted, i64::from(x), i64::from(y));
    canvas
}

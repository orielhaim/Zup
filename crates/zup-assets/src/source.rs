//! Decode a source once. Later sizes render from this, not from each other.

use std::io::Cursor;

use image::RgbaImage;

use crate::IconFormat;
use crate::error::IconError;

/// Largest source this pipeline will decode. Larger files are a mistake, not a
/// target to scale down from.
pub(crate) const MAX_SOURCE_PIXELS: u32 = 8192;

pub(crate) enum Normalized {
    Svg(Box<usvg::Tree>),
    Raster(RgbaImage),
}

impl Normalized {
    pub(crate) fn is_svg(&self) -> bool {
        matches!(self, Self::Svg(_))
    }

    pub(crate) fn view_size(&self) -> (f32, f32) {
        match self {
            Self::Svg(tree) => {
                let size = tree.size();
                (size.width(), size.height())
            }
            Self::Raster(image) => (image.width() as f32, image.height() as f32),
        }
    }

    pub(crate) fn raster_extent(&self) -> Option<(u32, u32)> {
        match self {
            Self::Raster(image) => Some((image.width(), image.height())),
            Self::Svg(_) => None,
        }
    }
}

pub(crate) fn normalize(
    bytes: &[u8],
    format: IconFormat,
    label: &str,
) -> Result<Normalized, IconError> {
    if bytes.is_empty() {
        return Err(IconError::Empty {
            label: label.to_owned(),
        });
    }
    match format {
        IconFormat::Svg => decode_svg(bytes, label),
        IconFormat::Png => decode_raster(bytes, image::ImageFormat::Png, label),
        IconFormat::WebP => decode_raster(bytes, image::ImageFormat::WebP, label),
        IconFormat::Ico => decode_ico(bytes, label),
        IconFormat::Icns => decode_icns(bytes, label),
    }
}

fn decode_svg(bytes: &[u8], label: &str) -> Result<Normalized, IconError> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).map_err(|error| {
        IconError::Svg {
            label: label.to_owned(),
            detail: error.to_string(),
        }
    })?;
    let size = tree.size();
    if !(size.width() > 0.0
        && size.height() > 0.0
        && size.width().is_finite()
        && size.height().is_finite())
    {
        return Err(IconError::ZeroSize {
            label: label.to_owned(),
        });
    }
    Ok(Normalized::Svg(Box::new(tree)))
}

fn decode_raster(
    bytes: &[u8],
    format: image::ImageFormat,
    label: &str,
) -> Result<Normalized, IconError> {
    let image = image::load_from_memory_with_format(bytes, format)
        .map_err(|error| IconError::Raster {
            label: label.to_owned(),
            detail: error.to_string(),
        })?
        .into_rgba8();
    check_extent(image.width(), image.height(), label)?;
    Ok(Normalized::Raster(image))
}

fn decode_ico(bytes: &[u8], label: &str) -> Result<Normalized, IconError> {
    let dir = ico::IconDir::read(Cursor::new(bytes)).map_err(|error| IconError::Raster {
        label: label.to_owned(),
        detail: error.to_string(),
    })?;
    let largest = dir
        .entries()
        .iter()
        .max_by_key(|entry| {
            (
                u64::from(entry.width()) * u64::from(entry.height()),
                entry.data().len(),
            )
        })
        .ok_or_else(|| IconError::Raster {
            label: label.to_owned(),
            detail: "the file contains no images".to_owned(),
        })?;
    let image = largest.decode().map_err(|error| IconError::Raster {
        label: label.to_owned(),
        detail: error.to_string(),
    })?;
    let width = image.width();
    let height = image.height();
    check_extent(width, height, label)?;
    RgbaImage::from_raw(width, height, image.into_rgba_data())
        .map(Normalized::Raster)
        .ok_or_else(|| IconError::Raster {
            label: label.to_owned(),
            detail: "an entry did not contain a full image".to_owned(),
        })
}

fn decode_icns(bytes: &[u8], label: &str) -> Result<Normalized, IconError> {
    let family = icns::IconFamily::read(Cursor::new(bytes)).map_err(|error| IconError::Raster {
        label: label.to_owned(),
        detail: error.to_string(),
    })?;
    let largest = family
        .available_icons()
        .into_iter()
        .max_by_key(|icon_type| icon_type.pixel_width() * icon_type.pixel_height())
        .ok_or_else(|| IconError::Raster {
            label: label.to_owned(),
            detail: "the file contains no images".to_owned(),
        })?;
    let image = family
        .get_icon_with_type(largest)
        .map_err(|error| IconError::Raster {
            label: label.to_owned(),
            detail: error.to_string(),
        })?;
    let width = image.width();
    let height = image.height();
    check_extent(width, height, label)?;
    let rgba = image
        .convert_to(icns::PixelFormat::RGBA)
        .into_data()
        .into_vec();
    RgbaImage::from_raw(width, height, rgba)
        .map(Normalized::Raster)
        .ok_or_else(|| IconError::Raster {
            label: label.to_owned(),
            detail: "an entry did not contain a full image".to_owned(),
        })
}

fn check_extent(width: u32, height: u32, label: &str) -> Result<(), IconError> {
    if width == 0 || height == 0 {
        return Err(IconError::ZeroSize {
            label: label.to_owned(),
        });
    }
    if width > MAX_SOURCE_PIXELS || height > MAX_SOURCE_PIXELS {
        return Err(IconError::TooLarge {
            label: label.to_owned(),
            width,
            height,
            limit: MAX_SOURCE_PIXELS,
        });
    }
    Ok(())
}

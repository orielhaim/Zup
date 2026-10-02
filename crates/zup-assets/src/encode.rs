//! Pack rendered squares into the container a target installs.

use std::collections::BTreeMap;
use std::io::Cursor;

use base64::Engine;
use image::{ImageEncoder, RgbaImage};

use crate::error::IconError;
use crate::{ExecutableIcon, IconRole};

pub(crate) const WINDOWS_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];
pub(crate) const MACOS_SIZES: &[u32] = &[16, 32, 64, 128, 256, 512, 1024];
pub(crate) const LINUX_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256, 512];

pub(crate) fn png(image: &RgbaImage) -> Result<Vec<u8>, IconError> {
    let mut bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    encoder
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| IconError::Encode {
            kind: "PNG",
            detail: error.to_string(),
        })?;
    Ok(bytes)
}

pub(crate) fn windows_ico(
    rendered: &BTreeMap<u32, RgbaImage>,
) -> Result<(Vec<u8>, ExecutableIcon), IconError> {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in WINDOWS_SIZES {
        let image = rendered.get(size).ok_or(IconError::Encode {
            kind: "ICO",
            detail: format!("missing {size}px render"),
        })?;
        let encoded = ico::IconImage::from_rgba_data(*size, *size, image.as_raw().clone());
        // PNG keeps the alpha channel. A BMP encoder will drop it to a palette
        // whenever the image looks simple, which is most icons.
        let entry =
            ico::IconDirEntry::encode_as_png(&encoded).map_err(|error| IconError::Encode {
                kind: "ICO",
                detail: error.to_string(),
            })?;
        dir.add_entry(entry);
    }
    let mut file = Vec::new();
    dir.write(&mut file).map_err(|error| IconError::Encode {
        kind: "ICO",
        detail: error.to_string(),
    })?;
    let executable = resources_from_ico(&file)?;
    Ok((file, executable))
}

pub(crate) fn resources_from_ico(bytes: &[u8]) -> Result<ExecutableIcon, IconError> {
    let dir = ico::IconDir::read(Cursor::new(bytes)).map_err(|error| IconError::Encode {
        kind: "ICO",
        detail: error.to_string(),
    })?;
    let mut images = Vec::with_capacity(dir.entries().len());
    let mut described = Vec::with_capacity(dir.entries().len());
    for entry in dir.entries() {
        described.push((entry.width().max(entry.height()), entry.data().len()));
        images.push(entry.data().to_vec());
    }
    if images.is_empty() {
        return Err(IconError::Encode {
            kind: "ICO",
            detail: "the icon contains no images".to_owned(),
        });
    }
    Ok(ExecutableIcon {
        images,
        group: group_directory(&described),
    })
}

/// `GRPICONDIR`: the same directory an ICO carries, except each entry names a
/// resource id instead of a file offset.
fn group_directory(entries: &[(u32, usize)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + entries.len() * 14);
    out.extend(0u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(
        u16::try_from(entries.len())
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    for (index, (size, length)) in entries.iter().enumerate() {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0);
        out.push(0);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend(u32::try_from(*length).unwrap_or(u32::MAX).to_le_bytes());
        out.extend((u16::try_from(index + 1).unwrap_or(u16::MAX)).to_le_bytes());
    }
    out
}

pub(crate) fn macos_icns(rendered: &BTreeMap<u32, RgbaImage>) -> Result<Vec<u8>, IconError> {
    let mut family = icns::IconFamily::new();
    for size in MACOS_SIZES {
        let image = rendered.get(size).ok_or(IconError::Encode {
            kind: "ICNS",
            detail: format!("missing {size}px render"),
        })?;
        let mut icon = icns::Image::new(icns::PixelFormat::RGBA, *size, *size);
        icon.data_mut().copy_from_slice(image.as_raw());
        family.add_icon(&icon).map_err(|error| IconError::Encode {
            kind: "ICNS",
            detail: error.to_string(),
        })?;
    }
    let mut bytes = Vec::new();
    family
        .write(&mut bytes)
        .map_err(|error| IconError::Encode {
            kind: "ICNS",
            detail: error.to_string(),
        })?;
    Ok(bytes)
}

pub(crate) fn linux_svg(bytes: &[u8], padding: f32, view_w: f32, view_h: f32) -> Vec<u8> {
    if padding == 0.0 {
        return bytes.to_vec();
    }
    let canvas = 1000.0f32;
    let avail = canvas * (1.0 - 2.0 * padding);
    let scale = (avail / view_w).min(avail / view_h);
    let width = view_w * scale;
    let height = view_h * scale;
    let x = (canvas - width) / 2.0;
    let y = (canvas - height) / 2.0;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 1000"><image x="{x:.4}" y="{y:.4}" width="{width:.4}" height="{height:.4}" href="data:image/svg+xml;base64,{encoded}"/></svg>"#
    )
    .into_bytes()
}

pub(crate) fn linux_name(app_id: &str, role: &IconRole) -> String {
    match role {
        IconRole::LinuxSvg => format!("hicolor/scalable/apps/{app_id}.svg"),
        IconRole::LinuxPng { size } => format!("hicolor/{size}x{size}/apps/{app_id}.png"),
        IconRole::Windows => "app.ico".to_owned(),
        IconRole::MacOs => "app.icns".to_owned(),
        IconRole::Png { size } => format!("png/{size}.png"),
    }
}

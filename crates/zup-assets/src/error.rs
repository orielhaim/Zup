//! Failures while reading or encoding an icon, written so a manifest author
//! can tell what to change.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IconError {
    #[error("icon `{label}` is empty")]
    Empty { label: String },

    #[error("icon `{label}` has no drawable pixels")]
    ZeroSize { label: String },

    #[error(
        "icon `{label}` uses `.{extension}`, which is not a supported icon format. Use SVG, PNG, or WebP"
    )]
    Unsupported { label: String, extension: String },

    #[error(
        "icon `{label}` is not a valid SVG: {detail}. Keep the file self-contained, with no external images or fonts"
    )]
    Svg { label: String, detail: String },

    #[error(
        "icon `{label}` could not be decoded: {detail}. Replace it with a valid PNG, WebP, ICO, or ICNS file"
    )]
    Raster { label: String, detail: String },

    #[error(
        "icon `{label}` is {width}×{height}, above the {limit} pixel limit. Export a smaller source"
    )]
    TooLarge {
        label: String,
        width: u32,
        height: u32,
        limit: u32,
    },

    #[error("icon padding must be at least 0 and less than 0.5")]
    Padding,

    #[error(
        "application id `{app_id}` cannot be used as an icon file name. Use an id without slashes or control characters"
    )]
    AppId { app_id: String },

    #[error(
        "linux icons were requested for both `{first}` and `{second}`. Compile one application at a time"
    )]
    LinuxIds { first: String, second: String },

    #[error("could not encode the {kind} icon: {detail}")]
    Encode { kind: &'static str, detail: String },

    #[error(
        "a {size} pixel icon is not a size this pipeline can emit. Use a size from 1 to {limit}"
    )]
    Size { size: u32, limit: u32 },

    #[error("icon cache: {0}")]
    Cache(String),
}

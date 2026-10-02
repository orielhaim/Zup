//! Compile one source icon into the files a build target installs.
//!
//! SVG is rasterized at every requested size. A raster source is fitted onto a
//! transparent square, preserving its aspect ratio, and scaled once per size.
//! The same source bytes and the same request produce the same files.

#![forbid(unsafe_code)]

mod cache;
mod encode;
mod error;
mod render;
mod source;

pub use cache::IconCache;
pub use error::IconError;

/// The Zup mark. Platform icons are generated from these bytes; nothing in the
/// repository keeps a pre-rendered copy as the source of truth.
pub const ZUP_ICON_SVG: &[u8] = include_bytes!("../assets/zup.svg");

/// Largest square this pipeline will emit.
pub const MAX_ICON_SIZE: u32 = 1024;

/// How the source bytes are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconFormat {
    Svg,
    Png,
    WebP,
    Ico,
    Icns,
}

/// One source image and the inset applied while fitting it to a square.
#[derive(Debug, Clone, Copy)]
pub struct IconSource<'a> {
    /// Path or other label used in diagnostics.
    pub label: &'a str,
    pub bytes: &'a [u8],
    pub format: IconFormat,
    /// Fraction of the canvas left empty on each side. `0.10` insets by 10%.
    pub padding: f32,
}

/// What a target needs. A build passes one of these per target, so a Windows
/// build never asks for a macOS or Linux icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconTarget {
    Windows,
    MacOs,
    Linux { app_id: String },
    Png { size: u32 },
}

impl IconTarget {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Windows => "windows".to_owned(),
            Self::MacOs => "macos".to_owned(),
            Self::Linux { app_id } => format!("linux\0{app_id}"),
            Self::Png { size } => format!("png\0{size}"),
        }
    }
}

/// Which generated file this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconRole {
    Windows,
    MacOs,
    LinuxSvg,
    LinuxPng { size: u32 },
    Png { size: u32 },
}

/// Image payloads an executable resource table stores for [`IconRole::Windows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableIcon {
    /// One image per icon, in resource-id order starting at 1.
    pub images: Vec<Vec<u8>>,
    /// The icon-group directory that points at those ids.
    pub group: Vec<u8>,
}

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconArtifact {
    pub role: IconRole,
    /// `/`-separated name relative to the target's icon output.
    pub name: String,
    /// Where a cache stored the file. Empty when this result was not cached.
    pub path: std::path::PathBuf,
    pub bytes: Vec<u8>,
    pub executable: Option<ExecutableIcon>,
}

/// Every file the requested targets need, plus warnings that did not stop the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledIcons {
    pub artifacts: Vec<IconArtifact>,
    pub warnings: Vec<String>,
}

/// The format implied by a file name.
pub fn format_of(path: &str) -> Result<IconFormat, IconError> {
    let extension = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let extension = extension.rsplit('.').next().unwrap_or("");
    if extension == path || extension.is_empty() {
        return Err(IconError::Unsupported {
            label: path.to_owned(),
            extension: String::new(),
        });
    }
    match extension.to_ascii_lowercase().as_str() {
        "svg" => Ok(IconFormat::Svg),
        "png" => Ok(IconFormat::Png),
        "webp" => Ok(IconFormat::WebP),
        "ico" => Ok(IconFormat::Ico),
        "icns" => Ok(IconFormat::Icns),
        other => Err(IconError::Unsupported {
            label: path.to_owned(),
            extension: other.to_owned(),
        }),
    }
}

/// Compile `source` into the union of `targets`.
///
/// Sizes are rendered once and then packed. Passing Windows and Linux yields
/// an ICO and the Linux tree, and does not yield an ICNS.
pub fn compile(
    source: &IconSource<'_>,
    targets: &[IconTarget],
) -> Result<CompiledIcons, IconError> {
    let padding_milli = quantize_padding(source.padding)?;
    let padding = f32::from(padding_milli) / 1000.0;
    let normalized = source::normalize(source.bytes, source.format, source.label)?;
    let plan = plan_targets(targets, normalized.is_svg())?;
    let mut rendered = std::collections::BTreeMap::new();
    for size in &plan.sizes {
        rendered.insert(
            *size,
            render::square(&normalized, *size, padding, source.label)?,
        );
    }
    let mut warnings = Vec::new();
    if let Some(warning) = small_source_warning(&normalized, &plan.sizes, source.label) {
        warnings.push(warning);
    }
    let mut artifacts = Vec::new();
    if plan.windows {
        let (bytes, executable) = encode::windows_ico(&rendered)?;
        artifacts.push(artifact(
            IconRole::Windows,
            "app.ico",
            bytes,
            Some(executable),
        ));
    }
    if plan.macos {
        let bytes = encode::macos_icns(&rendered)?;
        artifacts.push(artifact(IconRole::MacOs, "app.icns", bytes, None));
    }
    if let Some(app_id) = &plan.linux {
        if normalized.is_svg() {
            let (width, height) = normalized.view_size();
            let bytes = encode::linux_svg(source.bytes, padding, width, height);
            let role = IconRole::LinuxSvg;
            artifacts.push(artifact(
                role,
                &encode::linux_name(app_id, &role),
                bytes,
                None,
            ));
        }
        for size in encode::LINUX_SIZES {
            let role = IconRole::LinuxPng { size: *size };
            let bytes = encode::png(rendered.get(size).expect("linux size was rendered"))?;
            artifacts.push(artifact(
                role,
                &encode::linux_name(app_id, &role),
                bytes,
                None,
            ));
        }
    }
    for size in &plan.pngs {
        let role = IconRole::Png { size: *size };
        let bytes = encode::png(rendered.get(size).expect("png size was rendered"))?;
        artifacts.push(artifact(role, &encode::linux_name("", &role), bytes, None));
    }
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(CompiledIcons {
        artifacts,
        warnings,
    })
}

pub(crate) fn quantize_padding(padding: f32) -> Result<u16, IconError> {
    if !padding.is_finite() || padding < 0.0 || padding >= 0.5 {
        return Err(IconError::Padding);
    }
    let milli = (padding * 1000.0).round();
    if !(0.0..500.0).contains(&milli) {
        return Err(IconError::Padding);
    }
    Ok(milli as u16)
}

struct Plan {
    windows: bool,
    macos: bool,
    linux: Option<String>,
    pngs: std::collections::BTreeSet<u32>,
    sizes: std::collections::BTreeSet<u32>,
}

fn plan_targets(targets: &[IconTarget], _svg: bool) -> Result<Plan, IconError> {
    let mut plan = Plan {
        windows: false,
        macos: false,
        linux: None,
        pngs: std::collections::BTreeSet::new(),
        sizes: std::collections::BTreeSet::new(),
    };
    for target in targets {
        match target {
            IconTarget::Windows => {
                plan.windows = true;
                plan.sizes.extend(encode::WINDOWS_SIZES.iter().copied());
            }
            IconTarget::MacOs => {
                plan.macos = true;
                plan.sizes.extend(encode::MACOS_SIZES.iter().copied());
            }
            IconTarget::Linux { app_id } => {
                validate_app_id(app_id)?;
                if let Some(existing) = &plan.linux
                    && existing != app_id
                {
                    return Err(IconError::LinuxIds {
                        first: existing.clone(),
                        second: app_id.clone(),
                    });
                }
                plan.linux = Some(app_id.clone());
                plan.sizes.extend(encode::LINUX_SIZES.iter().copied());
            }
            IconTarget::Png { size } => {
                if *size == 0 || *size > MAX_ICON_SIZE {
                    return Err(IconError::Size {
                        size: *size,
                        limit: MAX_ICON_SIZE,
                    });
                }
                plan.pngs.insert(*size);
                plan.sizes.insert(*size);
            }
        }
    }
    Ok(plan)
}

fn validate_app_id(app_id: &str) -> Result<(), IconError> {
    let bad = app_id.is_empty()
        || app_id == "."
        || app_id == ".."
        || app_id.contains(['/', '\\', '\0'])
        || app_id.chars().any(char::is_control);
    if bad {
        Err(IconError::AppId {
            app_id: app_id.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn small_source_warning(
    source: &source::Normalized,
    sizes: &std::collections::BTreeSet<u32>,
    label: &str,
) -> Option<String> {
    let (width, height) = source.raster_extent()?;
    let largest = sizes.iter().copied().max()?;
    if largest <= width.max(height) {
        return None;
    }
    Some(format!(
        "{label} is {width}×{height}. A requested icon is {largest}×{largest}. Use an SVG, or a raster at least that large, so the icon stays sharp. This build will scale the file up."
    ))
}

fn artifact(
    role: IconRole,
    name: &str,
    bytes: Vec<u8>,
    executable: Option<ExecutableIcon>,
) -> IconArtifact {
    IconArtifact {
        role,
        name: name.to_owned(),
        path: std::path::PathBuf::new(),
        bytes,
        executable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    const WIDE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10"><rect width="20" height="10" fill="#ff0000"/></svg>"##;
    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#00ff00"/></svg>"##;

    fn svg<'a>(label: &'a str, text: &'a str, padding: f32) -> IconSource<'a> {
        IconSource {
            label,
            bytes: text.as_bytes(),
            format: IconFormat::Svg,
            padding,
        }
    }

    fn png_of(artifact: &IconArtifact) -> image::RgbaImage {
        image::load_from_memory(&artifact.bytes)
            .expect("png")
            .to_rgba8()
    }

    fn find<'a>(icons: &'a CompiledIcons, role: &IconRole) -> &'a IconArtifact {
        icons
            .artifacts
            .iter()
            .find(|artifact| &artifact.role == role)
            .unwrap_or_else(|| panic!("missing {role:?}"))
    }

    #[test]
    fn an_svg_becomes_a_multi_resolution_ico() {
        let icons = compile(&svg("mark.svg", SQUARE, 0.0), &[IconTarget::Windows]).unwrap();
        let ico = find(&icons, &IconRole::Windows);
        assert!(icons.warnings.is_empty());
        assert!(
            icons
                .artifacts
                .iter()
                .all(|artifact| artifact.role == IconRole::Windows)
        );
        let executable = ico.executable.as_ref().unwrap();
        assert_eq!(executable.images.len(), encode::WINDOWS_SIZES.len());
        assert_eq!(&executable.group[0..4], &[0, 0, 1, 0]);
        assert_eq!(executable.group[4], encode::WINDOWS_SIZES.len() as u8);
        assert!(
            executable
                .images
                .iter()
                .all(|image| image.starts_with(b"\x89PNG"))
        );
        let read = ico::IconDir::read(std::io::Cursor::new(&ico.bytes)).unwrap();
        assert_eq!(read.entries().len(), encode::WINDOWS_SIZES.len());
    }

    #[test]
    fn an_svg_becomes_an_icns() {
        let icons = compile(&svg("mark.svg", SQUARE, 0.0), &[IconTarget::MacOs]).unwrap();
        assert_eq!(icons.artifacts.len(), 1);
        assert_eq!(icons.artifacts[0].role, IconRole::MacOs);
        assert!(icons.artifacts[0].bytes.starts_with(b"icns"));
        assert!(icons.artifacts[0].executable.is_none());
    }

    #[test]
    fn an_svg_becomes_a_hicolor_tree_and_keeps_the_source() {
        let icons = compile(
            &svg("mark.svg", SQUARE, 0.0),
            &[IconTarget::Linux {
                app_id: "com.example.acme".into(),
            }],
        )
        .unwrap();
        let scalable = find(&icons, &IconRole::LinuxSvg);
        assert_eq!(scalable.name, "hicolor/scalable/apps/com.example.acme.svg");
        assert_eq!(scalable.bytes, SQUARE.as_bytes());
        assert_eq!(
            icons
                .artifacts
                .iter()
                .filter(|artifact| matches!(artifact.role, IconRole::LinuxPng { .. }))
                .count(),
            encode::LINUX_SIZES.len()
        );
        assert!(icons.artifacts.iter().all(|artifact| {
            matches!(
                artifact.role,
                IconRole::LinuxSvg | IconRole::LinuxPng { .. }
            )
        }));
    }

    #[test]
    fn windows_and_linux_do_not_pull_in_macos() {
        let icons = compile(
            &svg("mark.svg", SQUARE, 0.0),
            &[
                IconTarget::Linux {
                    app_id: "com.example.acme".into(),
                },
                IconTarget::Windows,
            ],
        )
        .unwrap();
        assert!(
            icons
                .artifacts
                .iter()
                .any(|artifact| artifact.role == IconRole::Windows)
        );
        assert!(
            icons
                .artifacts
                .iter()
                .any(|artifact| artifact.role == IconRole::LinuxSvg)
        );
        assert!(
            icons
                .artifacts
                .iter()
                .all(|artifact| artifact.role != IconRole::MacOs)
        );
    }

    #[test]
    fn a_wide_svg_is_letterboxed_instead_of_stretched() {
        let icons = compile(&svg("wide.svg", WIDE, 0.0), &[IconTarget::Png { size: 40 }]).unwrap();
        let image = png_of(&icons.artifacts[0]);
        assert_eq!(image.dimensions(), (40, 40));
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(image.get_pixel(20, 20).0[0], 255);
        assert_eq!(image.get_pixel(20, 20).0[3], 255);
        assert_eq!(image.get_pixel(0, 20).0[0], 255);
    }

    #[test]
    fn padding_insets_the_mark_and_leaves_the_corner_clear() {
        let icons = compile(
            &svg("mark.svg", SQUARE, 0.25),
            &[IconTarget::Png { size: 40 }],
        )
        .unwrap();
        let image = png_of(&icons.artifacts[0]);
        assert_eq!(image.get_pixel(0, 0).0[3], 0);
        assert_eq!(image.get_pixel(20, 20).0[1], 255);
    }

    #[test]
    fn a_raster_keeps_transparency_and_warns_when_it_is_too_small() {
        let mut image = image::RgbaImage::from_pixel(32, 32, image::Rgba([9, 8, 7, 255]));
        image.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), 32, 32, image::ExtendedColorType::Rgba8)
            .unwrap();
        let icons = compile(
            &IconSource {
                label: "assets/icon.png",
                bytes: &bytes,
                format: IconFormat::Png,
                padding: 0.0,
            },
            &[IconTarget::Png { size: 32 }, IconTarget::Png { size: 64 }],
        )
        .unwrap();
        assert!(icons.warnings[0].contains("32×32"));
        assert!(icons.warnings[0].contains("64×64"));
        let exact = find(&icons, &IconRole::Png { size: 32 });
        let rendered = png_of(exact);
        assert_eq!(rendered.get_pixel(0, 0).0[3], 0);
        assert_eq!(rendered.get_pixel(16, 16).0, [9, 8, 7, 255]);
    }

    #[test]
    fn the_same_source_compiles_to_the_same_bytes() {
        let first = compile(
            &svg("mark.svg", ZUP_ICON_SVG_TEXT, 0.0),
            &[IconTarget::Windows],
        )
        .unwrap();
        let second = compile(
            &svg("mark.svg", ZUP_ICON_SVG_TEXT, 0.0),
            &[IconTarget::Windows],
        )
        .unwrap();
        assert_eq!(first.artifacts[0].bytes, second.artifacts[0].bytes);
    }

    const ZUP_ICON_SVG_TEXT: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 192 128" color="#FFF200"><path fill="currentColor" d="M192 0L62 4L16 40H104L0 128L144 124L192 84H104Z"/></svg>"##;

    #[test]
    fn the_built_in_mark_compiles_for_every_target() {
        let source = IconSource {
            label: "zup.svg",
            bytes: ZUP_ICON_SVG,
            format: IconFormat::Svg,
            padding: 0.0,
        };
        let icons = compile(
            &source,
            &[
                IconTarget::Windows,
                IconTarget::MacOs,
                IconTarget::Linux {
                    app_id: "dev.zup.zup".into(),
                },
            ],
        )
        .unwrap();
        assert!(icons.warnings.is_empty());
        assert!(find(&icons, &IconRole::Windows).bytes.len() > 64);
        assert!(find(&icons, &IconRole::MacOs).bytes.len() > 64);
        assert_eq!(
            find(&icons, &IconRole::LinuxSvg).name,
            "hicolor/scalable/apps/dev.zup.zup.svg"
        );
    }

    #[test]
    fn a_cache_hit_does_not_rewrite_the_file() {
        let root = tempfile::tempdir().unwrap();
        let cache = IconCache::open(root.path());
        let source = svg("mark.svg", SQUARE, 0.0);
        let first = cache.compile(&source, &[IconTarget::Windows]).unwrap();
        let path = first.artifacts[0].path.clone();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        let second = cache.compile(&source, &[IconTarget::Windows]).unwrap();
        assert_eq!(first.artifacts[0].bytes, second.artifacts[0].bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
    }

    #[test]
    fn a_changed_source_is_not_served_from_the_previous_entry() {
        let root = tempfile::tempdir().unwrap();
        let cache = IconCache::open(root.path());
        let first = cache
            .compile(
                &svg("mark.svg", SQUARE, 0.0),
                &[IconTarget::Png { size: 16 }],
            )
            .unwrap();
        let second = cache
            .compile(&svg("mark.svg", WIDE, 0.0), &[IconTarget::Png { size: 16 }])
            .unwrap();
        assert_ne!(first.artifacts[0].bytes, second.artifacts[0].bytes);
    }

    #[test]
    fn invalid_input_names_the_fix() {
        let error = compile(
            &IconSource {
                label: "icon.bmp",
                bytes: b"nope",
                format: IconFormat::Png,
                padding: 0.0,
            },
            &[IconTarget::Windows],
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("could not be decoded"),
            "{error}"
        );

        let error = format_of("assets/icon.bmp").unwrap_err();
        assert!(error.to_string().contains("SVG"), "{error}");

        let error = compile(&svg("mark.svg", SQUARE, 0.8), &[IconTarget::Windows]).unwrap_err();
        assert!(error.to_string().contains("padding"), "{error}");

        let error = compile(&svg("mark.svg", "<svg", 0.0), &[IconTarget::Windows]).unwrap_err();
        assert!(error.to_string().contains("SVG"), "{error}");
    }

    #[test]
    fn an_ico_source_uses_its_largest_image() {
        let mut pixels = image::RgbaImage::new(32, 32);
        pixels.put_pixel(16, 16, image::Rgba([0, 0, 255, 255]));
        let small = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]));
        let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
        for image in [small, pixels] {
            let encoded =
                ico::IconImage::from_rgba_data(image.width(), image.height(), image.into_raw());
            dir.add_entry(ico::IconDirEntry::encode(&encoded).unwrap());
        }
        let mut bytes = Vec::new();
        dir.write(&mut bytes).unwrap();
        let icons = compile(
            &IconSource {
                label: "app.ico",
                bytes: &bytes,
                format: IconFormat::Ico,
                padding: 0.0,
            },
            &[IconTarget::Png { size: 32 }],
        )
        .unwrap();
        let image = png_of(&icons.artifacts[0]);
        assert_eq!(image.get_pixel(16, 16).0[2], 255);
        assert!(icons.warnings.is_empty());
    }

    #[test]
    fn an_icns_source_uses_its_largest_image() {
        let icons = compile(&svg("mark.svg", SQUARE, 0.0), &[IconTarget::MacOs]).unwrap();
        let icns = find(&icons, &IconRole::MacOs).bytes.clone();
        let icons = compile(
            &IconSource {
                label: "app.icns",
                bytes: &icns,
                format: IconFormat::Icns,
                padding: 0.0,
            },
            &[IconTarget::Png {
                size: MAX_ICON_SIZE,
            }],
        )
        .unwrap();
        let image = png_of(&icons.artifacts[0]);
        assert_eq!(
            image.dimensions(),
            (MAX_ICON_SIZE, MAX_ICON_SIZE),
            "the family should not be padded or letterboxed"
        );
        assert_eq!(image.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert!(icons.warnings.is_empty());
    }
}

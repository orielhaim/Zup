use std::path::{Path, PathBuf};

use zup_bundle::{Package, PackageError};
use zup_core::{Sha256Digest, Target};

pub use zup_compose::{CARRIER_FOOTER_LEN as FOOTER_LEN, CARRIER_MAGIC, CARRIER_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum CarrierError {
    #[error("`{path}` is {size} bytes, too short to be a zup installer")]
    TooShort { path: String, size: u64 },

    #[error("`{path}` carries no zup package footer")]
    NoFooter { path: String },

    #[error(
        "`{path}` declares carrier format {found}, which this build does not understand (expected {expected})"
    )]
    UnsupportedVersion {
        path: String,
        found: u32,
        expected: u32,
    },

    #[error("`{path}` declares an unsupported footer layout ({flags:#x})")]
    UnsupportedFlags { path: String, flags: u32 },

    #[error("`{path}` declares its package at {offset}..{end}, outside the {size}-byte file")]
    PackageOutsideFile {
        path: String,
        offset: u64,
        end: u64,
        size: u64,
    },

    #[error("`{path}` declares a {length}-byte package, which is not plausible")]
    ImplausibleLength { path: String, length: u64 },

    #[error(
        "`{path}` carries a package that hashes to {found}, not the {expected} its footer declares"
    )]
    PackageDigestMismatch {
        path: String,
        expected: Sha256Digest,
        found: Sha256Digest,
    },

    #[error(
        "`{path}` carries a preset that hashes to {found}, not the {expected} its footer declares"
    )]
    PresetDigestMismatch {
        path: String,
        expected: Sha256Digest,
        found: Sha256Digest,
    },

    #[error("the package is not a valid zup package: {0}")]
    Package(#[from] PackageError),

    #[error("the carrier image is {found}, but the package installs {expected}")]
    TargetMismatch { found: String, expected: String },

    #[error("carrier I/O at `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl CarrierError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

impl From<zup_compose::ComposeError> for CarrierError {
    fn from(error: zup_compose::ComposeError) -> Self {
        use zup_compose::ComposeError as Source;
        match error {
            Source::Io { path, source } => Self::Io { path, source },
            Source::Inspect(error) => Self::Io {
                path: String::new(),
                source: std::io::Error::other(error.to_string()),
            },
            Source::Target(error) => Self::TargetMismatch {
                found: error.to_string(),
                expected: String::new(),
            },
            Source::Frontend(error) => Self::TargetMismatch {
                found: error.to_string(),
                expected: String::new(),
            },
            Source::Pe(error) => Self::Io {
                path: String::new(),
                source: std::io::Error::other(error.to_string()),
            },
            Source::Package(error) => Self::Package(error),
            Source::ResourceTooLarge { size, limit } => Self::Io {
                path: format!("package data is {size} bytes; the resource limit is {limit} bytes"),
                source: std::io::Error::other("package too large for composition"),
            },
            Source::TooManyBlobs { count, limit } => Self::Io {
                path: format!("package has {count} blobs; the limit is {limit}"),
                source: std::io::Error::other("package has too many blobs"),
            },
            Source::ResourceAllocation { size } => Self::Io {
                path: format!("cannot allocate {size} bytes"),
                source: std::io::Error::other("allocation failed"),
            },
            Source::RuntimeAlreadySigned => Self::Io {
                path: String::new(),
                source: std::io::Error::other("runtime is already signed"),
            },
            Source::TargetMismatch { expected, found } => Self::TargetMismatch {
                found: found.to_string(),
                expected: expected.to_string(),
            },
            Source::FormatMismatch {
                path,
                format,
                expected,
            } => Self::TargetMismatch {
                found: format,
                expected: format!("{expected} at {path}"),
            },
            Source::TooShort { path, size } => Self::TooShort { path, size },
            Source::NoContent { path } => Self::NoFooter { path },
            Source::UnsupportedVersion {
                path,
                found,
                expected,
            } => Self::UnsupportedVersion {
                path,
                found,
                expected,
            },
            Source::ContentOutsideFile {
                path,
                offset,
                end,
                size,
            } => Self::PackageOutsideFile {
                path,
                offset,
                end,
                size,
            },
            Source::ImplausibleLength { path, length } => Self::ImplausibleLength { path, length },
            Source::PackageDigestMismatch {
                path,
                expected,
                found,
            } => Self::PackageDigestMismatch {
                path,
                expected,
                found,
            },
            Source::PresetDigestMismatch {
                path,
                expected,
                found,
            } => Self::PresetDigestMismatch {
                path,
                expected,
                found,
            },
            Source::OutputIsRuntime => Self::Io {
                path: String::new(),
                source: std::io::Error::other(
                    "refusing to compose an installer over its own runtime template",
                ),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarrierFooter {
    pub version: u32,
    pub flags: u32,

    pub package_offset: u64,

    pub package_length: u64,

    pub package_digest: Sha256Digest,

    pub preset_length: u64,

    pub preset_digest: Sha256Digest,
}

impl CarrierFooter {
    pub fn package_range(&self, file_size: u64) -> Result<std::ops::Range<u64>, (u64, u64)> {
        let end = self
            .package_offset
            .checked_add(self.package_length)
            .ok_or((self.package_offset, u64::MAX))?;
        if end > file_size {
            return Err((self.package_offset, end));
        }
        Ok(self.package_offset..end)
    }
}

#[derive(Debug)]
pub struct Carrier {
    executable: PathBuf,
    package: Package,
    preset: Option<Vec<u8>>,
}

impl Carrier {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CarrierError> {
        let executable = path.as_ref().to_path_buf();
        let opened = zup_compose::open_linux(&executable)?;
        Ok(Self {
            executable,
            package: opened.package,
            preset: opened.preset,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn package(&self) -> &Package {
        &self.package
    }

    pub fn preset(&self) -> Option<&[u8]> {
        self.preset.as_deref()
    }

    pub fn into_package(self) -> Package {
        self.package
    }

    pub fn target(&self) -> &Target {
        &self.package.plan().installer.target
    }
}

pub fn compose(
    runtime: &Path,
    output: &Path,
    package: &[u8],
) -> Result<CarrierFooter, CarrierError> {
    compose_with_preset(runtime, output, package, None)
}

pub fn compose_with_preset(
    runtime: &Path,
    output: &Path,
    package: &[u8],
    preset: Option<&[u8]>,
) -> Result<CarrierFooter, CarrierError> {
    let report = zup_compose::compose_linux(runtime, output, package, preset)?;
    let preset = preset.unwrap_or_default();
    Ok(CarrierFooter {
        version: CARRIER_VERSION,
        flags: 0,
        package_offset: report
            .output_size
            .checked_sub(report.package_size + report.preset_size + FOOTER_LEN)
            .ok_or_else(|| {
                CarrierError::io(output, std::io::Error::other("composed size underflow"))
            })?,
        package_length: report.package_size,
        package_digest: zup_core::hash_bytes(package),
        preset_length: report.preset_size,
        preset_digest: zup_core::hash_bytes(preset),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn runtime_bytes(marker: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes.extend_from_slice(marker.as_bytes());
        bytes.resize(512, 0);
        bytes
    }

    fn runtime_file(directory: &Path, marker: &str) -> PathBuf {
        let path = directory.join("runtime");
        std::fs::write(&path, runtime_bytes(marker)).expect("write runtime");
        path
    }

    #[test]
    fn a_composed_carrier_round_trips_through_its_footer() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        let package = b"a package".to_vec();

        let footer = compose(&runtime, &output, &package).expect("compose");

        assert_eq!(footer.package_length, package.len() as u64);
        assert_eq!(
            footer.package_offset,
            runtime_bytes("zup-installer").len() as u64,
            "the package begins exactly where the template ends"
        );
        assert_eq!(
            std::fs::metadata(&output).expect("stat").len(),
            footer.package_offset + footer.package_length + footer.preset_length + FOOTER_LEN,
            "the file is the template, the package, the preset, and the footer"
        );

        assert_eq!(footer.package_digest, zup_core::hash_bytes(&package));
        assert_eq!(footer.preset_length, 0);

        let error = Carrier::open(&output).expect_err("this fixture is not a real package");
        assert!(
            matches!(error, CarrierError::Package(_)),
            "the composed footer must be readable: {error:?}"
        );
    }

    #[test]
    fn a_composed_carrier_carries_its_preset() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        let footer =
            compose_with_preset(&runtime, &output, b"pkg", Some(b"preset")).expect("compose");
        assert_eq!(footer.preset_length, 6);
        assert_eq!(footer.preset_digest, zup_core::hash_bytes(b"preset"));
        let bytes = std::fs::read(&output).expect("read");
        let at = footer.package_offset as usize + footer.package_length as usize;
        assert_eq!(&bytes[at..at + 6], b"preset");
    }

    #[cfg(unix)]
    #[test]
    fn a_composed_carrier_is_executable_whatever_its_template_carries() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o644))
            .expect("strip the template");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"pkg").expect("compose");
        let mode = std::fs::metadata(&output)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "the installer is executable; the template's bits are not inherited"
        );
    }

    #[test]
    fn a_carriers_template_is_never_modified() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let before = std::fs::read(&runtime).expect("read");
        compose(&runtime, &directory.path().join("out"), b"pkg").expect("compose");
        assert_eq!(
            std::fs::read(&runtime).expect("read"),
            before,
            "composition copies; it never writes into the template"
        );
    }

    #[test]
    fn a_file_with_no_footer_is_refused_rather_than_scanned() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let path = directory.path().join("runtime");
        std::fs::write(&path, runtime_bytes("no footer here")).expect("write");
        assert!(matches!(
            Carrier::open(&path),
            Err(CarrierError::NoFooter { .. })
        ));
    }

    #[test]
    fn a_magic_inside_the_template_is_not_a_carrier() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let mut bytes = runtime_bytes("plain");
        bytes.extend_from_slice(CARRIER_MAGIC);
        bytes.extend_from_slice(b" and some more bytes to look like a footer");
        let path = directory.path().join("runtime");
        std::fs::write(&path, &bytes).expect("write");
        assert!(
            matches!(Carrier::open(&path), Err(CarrierError::NoFooter { .. })),
            "the footer has to be at the end, in the right place"
        );
    }

    #[rstest]
    #[case::shorter_than_a_footer(vec![0u8; 8], CarrierErrorKind::TooShort)]
    #[case::long_enough_but_not_a_carrier(
        vec![0u8; 200],
        CarrierErrorKind::NoFooter
    )]
    fn a_malformed_carrier_is_refused_without_panicking(
        #[case] bytes: Vec<u8>,
        #[case] expected: CarrierErrorKind,
    ) {
        let directory = tempfile::tempdir().expect("a temp directory");
        let path = directory.path().join("runtime");
        std::fs::write(&path, &bytes).expect("write");
        let error = Carrier::open(&path).expect_err("a malformed carrier is refused");
        assert!(
            matches!(
                (&error, expected),
                (CarrierError::TooShort { .. }, CarrierErrorKind::TooShort)
                    | (CarrierError::NoFooter { .. }, CarrierErrorKind::NoFooter)
            ),
            "{error:?}"
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CarrierErrorKind {
        TooShort,
        NoFooter,
    }

    #[test]
    fn every_declared_offset_is_checked_against_the_file() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"a package").expect("compose");
        let original = std::fs::read(&output).expect("read");

        let mut tampered = original.clone();
        footer_put(&mut tampered, 25, u64::MAX - 1);
        assert_outside(tampered);

        let mut tampered = original.clone();
        footer_put(&mut tampered, 25, u64::MAX);
        footer_put(&mut tampered, 33, 2);
        assert_outside(tampered);

        let mut tampered = original.clone();
        footer_put(&mut tampered, 33, u64::MAX / 2);
        assert_outside(tampered);

        let mut tampered = original.clone();
        footer_put(&mut tampered, 25, 0);
        footer_put(&mut tampered, 33, 1_000_000_000_000);
        assert_outside(tampered);

        assert!(
            matches!(
                raw_open(&original),
                Err(CarrierError::Package(_)) | Err(CarrierError::TargetMismatch { .. })
            ),
            "the composed footer must still be readable"
        );
    }

    #[test]
    fn edited_package_bytes_are_refused() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"the original package").expect("compose");

        let mut tampered = std::fs::read(&output).expect("read");
        tampered[runtime_bytes("zup-installer").len()] = b'X';

        let error = raw_open(&tampered).expect_err("edited bytes are refused");
        assert!(
            matches!(error, CarrierError::PackageDigestMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_footer_claiming_another_layout_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"a package").expect("compose");

        let mut tampered = std::fs::read(&output).expect("read");
        footer_put(&mut tampered, 73, 999);
        assert!(
            matches!(
                raw_open(&tampered),
                Err(CarrierError::PackageDigestMismatch { .. })
                    | Err(CarrierError::PackageOutsideFile { .. })
                    | Err(CarrierError::UnsupportedVersion { .. })
            ),
            "a tampered preset length must not decode as content"
        );
    }

    #[test]
    fn a_future_carrier_version_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"a package").expect("compose");

        let mut tampered = std::fs::read(&output).expect("read");
        let at = tampered.len() - FOOTER_LEN as usize + 17;
        tampered[at..at + 4].copy_from_slice(&99u32.to_le_bytes());
        assert!(
            matches!(
                raw_open(&tampered),
                Err(CarrierError::UnsupportedVersion { found: 99, .. })
            ),
            "a version this build does not know must not be parsed with this build's layout"
        );
    }

    fn raw_open(bytes: &[u8]) -> Result<Package, CarrierError> {
        let directory = tempfile::tempdir().expect("a temp directory");
        let path = directory.path().join("carrier");
        std::fs::write(&path, bytes).expect("write");
        Ok(Carrier::open(&path)?.into_package())
    }

    fn footer_put(bytes: &mut [u8], field_offset: usize, value: u64) {
        let at = bytes.len() - FOOTER_LEN as usize + field_offset;
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn assert_outside(bytes: Vec<u8>) {
        assert!(
            matches!(
                raw_open(&bytes),
                Err(CarrierError::PackageOutsideFile { .. })
            ),
            "an offset outside the file is refused"
        );
    }

    #[test]
    fn the_footer_carries_no_target_of_its_own() {
        assert_eq!(
            FOOTER_LEN,
            17 + 4 + 4 + 8 + 8 + 32 + 8 + 32 + 8,
            "magic, version, flags, offset, length, digest, preset length, preset digest, footer length"
        );
    }
}

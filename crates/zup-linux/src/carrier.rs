use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use zup_bundle::{Package, PackageError};
use zup_core::{Sha256Digest, TargetTriple};

pub const CARRIER_MAGIC: &[u8; 17] = b"ZUP-LINUX-CARRIER";

pub const CARRIER_VERSION: u32 = 1;

pub const FOOTER_LEN: u64 = 81;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarrierFooter {
    pub version: u32,
    pub flags: u32,

    pub package_offset: u64,

    pub package_length: u64,

    pub package_digest: Sha256Digest,
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

    fn to_bytes(self) -> [u8; FOOTER_LEN as usize] {
        let mut bytes = [0u8; FOOTER_LEN as usize];
        bytes[..17].copy_from_slice(CARRIER_MAGIC);
        bytes[17..21].copy_from_slice(&self.version.to_le_bytes());
        bytes[21..25].copy_from_slice(&self.flags.to_le_bytes());
        bytes[25..33].copy_from_slice(&self.package_offset.to_le_bytes());
        bytes[33..41].copy_from_slice(&self.package_length.to_le_bytes());
        bytes[41..73].copy_from_slice(self.package_digest.as_bytes());
        bytes[73..81].copy_from_slice(&FOOTER_LEN.to_le_bytes());
        bytes
    }
}

fn read_footer(
    file: &mut std::fs::File,
    path: &Path,
    file_size: u64,
) -> Result<CarrierFooter, CarrierError> {
    if file_size < FOOTER_LEN {
        return Err(CarrierError::TooShort {
            path: path.display().to_string(),
            size: file_size,
        });
    }
    file.seek(SeekFrom::Start(file_size - FOOTER_LEN))
        .map_err(|error| CarrierError::io(path, error))?;
    let mut bytes = [0u8; FOOTER_LEN as usize];
    file.read_exact(&mut bytes)
        .map_err(|error| CarrierError::io(path, error))?;

    if &bytes[..CARRIER_MAGIC.len()] != CARRIER_MAGIC.as_slice() {
        return Err(CarrierError::NoFooter {
            path: path.display().to_string(),
        });
    }
    let version = u32::from_le_bytes(bytes[17..21].try_into().expect("four bytes"));
    if version != CARRIER_VERSION {
        return Err(CarrierError::UnsupportedVersion {
            path: path.display().to_string(),
            found: version,
            expected: CARRIER_VERSION,
        });
    }
    let declared_len = u64::from_le_bytes(bytes[73..81].try_into().expect("eight bytes"));
    if declared_len != FOOTER_LEN {
        return Err(CarrierError::UnsupportedFlags {
            path: path.display().to_string(),
            flags: declared_len as u32,
        });
    }
    let package_offset = u64::from_le_bytes(bytes[25..33].try_into().expect("eight bytes"));
    let package_length = u64::from_le_bytes(bytes[33..41].try_into().expect("eight bytes"));
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&bytes[41..73]);

    let footer = CarrierFooter {
        version,
        flags: u32::from_le_bytes(bytes[21..25].try_into().expect("four bytes")),
        package_offset,
        package_length,
        package_digest: Sha256Digest::from_bytes(digest),
    };
    footer
        .package_range(file_size)
        .map_err(|(offset, end)| CarrierError::PackageOutsideFile {
            path: path.display().to_string(),
            offset,
            end,
            size: file_size,
        })?;

    Ok(footer)
}

#[derive(Debug)]
pub struct Carrier {
    executable: PathBuf,
    package: Package,
}

impl Carrier {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CarrierError> {
        let executable = path.as_ref().to_path_buf();
        let mut file = std::fs::File::open(&executable)
            .map_err(|error| CarrierError::io(path.as_ref(), error))?;
        let file_size = file
            .metadata()
            .map_err(|error| CarrierError::io(path.as_ref(), error))?
            .len();
        let footer = read_footer(&mut file, path.as_ref(), file_size)?;
        let range = footer.package_range(file_size).map_err(|(offset, end)| {
            CarrierError::PackageOutsideFile {
                path: path.as_ref().display().to_string(),
                offset,
                end,
                size: file_size,
            }
        })?;

        file.seek(SeekFrom::Start(range.start))
            .map_err(|error| CarrierError::io(path.as_ref(), error))?;
        let mut bytes = vec![0u8; usize::try_from(range.end - range.start).unwrap_or(usize::MAX)];
        file.read_exact(&mut bytes)
            .map_err(|error| CarrierError::io(path.as_ref(), error))?;

        let found = zup_core::hash_bytes(&bytes);
        if found != footer.package_digest {
            return Err(CarrierError::PackageDigestMismatch {
                path: path.as_ref().display().to_string(),
                expected: footer.package_digest,
                found,
            });
        }

        let package = Package::from_bytes(bytes)?;

        let expected = package.plan().installer.target.clone();
        let image =
            zup_binary::Executable::read(&executable).map_err(|error| CarrierError::Io {
                path: path.as_ref().display().to_string(),
                source: std::io::Error::other(error.to_string()),
            })?;
        image
            .refuse_target(&expected)
            .map_err(|error| CarrierError::TargetMismatch {
                found: error.to_string(),
                expected: expected.to_string(),
            })?;

        if image.format() != zup_binary::BinaryFormat::Elf
            || expected.operating_system() != zup_core::TargetOperatingSystem::Linux
        {
            return Err(CarrierError::TargetMismatch {
                found: image.format().to_string(),
                expected: expected.to_string(),
            });
        }

        Ok(Self {
            executable,
            package,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn package(&self) -> &Package {
        &self.package
    }

    pub fn into_package(self) -> Package {
        self.package
    }

    pub fn target(&self) -> &TargetTriple {
        &self.package.plan().installer.target
    }
}

pub fn compose(
    runtime: &Path,
    output: &Path,
    package: &[u8],
) -> Result<CarrierFooter, CarrierError> {
    if package.is_empty() {
        return Err(CarrierError::ImplausibleLength {
            path: output.display().to_string(),
            length: 0,
        });
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| CarrierError::io(parent, error))?;

    let mut image =
        std::fs::File::open(runtime).map_err(|error| CarrierError::io(runtime, error))?;
    let runtime_len = image
        .metadata()
        .map_err(|error| CarrierError::io(runtime, error))?
        .len();

    let temporary = output.with_extension("zup-partial");
    let composed = (|| -> Result<(), std::io::Error> {
        let mut out = std::fs::File::create(&temporary)?;
        std::io::copy(&mut image, &mut out)?;
        out.write_all(package)?;
        let footer = CarrierFooter {
            version: CARRIER_VERSION,
            flags: 0,
            package_offset: runtime_len,
            package_length: package.len() as u64,
            package_digest: zup_core::hash_bytes(package),
        };
        out.write_all(&footer.to_bytes())?;
        out.sync_all()
    })();
    if let Err(error) = composed {
        let _ = std::fs::remove_file(&temporary);
        return Err(CarrierError::io(output, error));
    }

    let bytes = std::fs::read(&temporary).map_err(|error| CarrierError::io(&temporary, error))?;
    zup_platform::publish(output, &bytes).map_err(|error| CarrierError::Io {
        path: output.display().to_string(),
        source: std::io::Error::other(error.to_string()),
    })?;
    let _ = std::fs::remove_file(&temporary);

    #[cfg(unix)]
    {
        let template_mode = std::fs::symlink_metadata(runtime)
            .map_err(|error| CarrierError::io(runtime, error))?
            .permissions()
            .mode()
            & 0o777;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(template_mode))
            .map_err(|error| CarrierError::io(output, error))?;
    }

    Ok(CarrierFooter {
        version: CARRIER_VERSION,
        flags: 0,
        package_offset: runtime_len,
        package_length: package.len() as u64,
        package_digest: zup_core::hash_bytes(package),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn runtime_bytes(marker: &str) -> Vec<u8> {
        let mut bytes = b"\x7fELF\x02\x01\x01\x00".to_vec();
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
            footer.package_offset + footer.package_length + FOOTER_LEN,
            "the file is the template, the package, and the footer - nothing else"
        );

        assert_eq!(footer.package_digest, zup_core::hash_bytes(&package));

        let error = Carrier::open(&output).expect_err("this fixture is not a real package");
        assert!(
            matches!(
                error,
                CarrierError::Package(_) | CarrierError::TargetMismatch { .. }
            ),
            "the composed footer must be readable: {error:?}"
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
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"a package").expect("compose");

        let mut tampered = std::fs::read(&output).expect("read");
        footer_put(&mut tampered, 73, 999);
        assert!(
            matches!(
                raw_open(&tampered),
                Err(CarrierError::UnsupportedFlags { .. })
            ),
            "a different footer layout is not this build's to read"
        );
    }

    #[test]
    fn a_future_carrier_version_is_refused() {
        let directory = tempfile::tempdir().expect("a temp directory");
        let runtime = runtime_file(directory.path(), "zup-installer");
        let output = directory.path().join("Acme-Setup");
        compose(&runtime, &output, b"a package").expect("compose");

        let mut tampered = std::fs::read(&output).expect("read");
        let at = tampered.len() - FOOTER_LEN as usize + 17;
        tampered[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(
            matches!(
                raw_open(&tampered),
                Err(CarrierError::UnsupportedVersion { found: 2, .. })
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
            17 + 4 + 4 + 8 + 8 + 32 + 8,
            "magic, version, flags, offset, length, digest, footer length"
        );
        let footer = CarrierFooter {
            version: CARRIER_VERSION,
            flags: 0,
            package_offset: 0,
            package_length: 0,
            package_digest: Sha256Digest::from_bytes([0; 32]),
        };
        assert_eq!(
            footer.to_bytes().len() as u64,
            FOOTER_LEN,
            "the fixed-width encoding is exactly the advertised footer size"
        );
    }
}

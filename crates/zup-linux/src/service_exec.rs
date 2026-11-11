use crate::error::ExecError;
use crate::locations::MachineRoots;
use crate::machine::SystemdRoots;
use crate::systemd::{SystemdManager, UnitChange};
use zup_core::ServiceStart;

pub struct ServiceContext<'a> {
    pub controlled_units: &'a [String],
    pub roots: &'a MachineRoots,
    pub systemd: &'a SystemdRoots,
    pub manager: &'a mut dyn SystemdManager,
    pub expected_uid: u32,
}

pub(crate) fn apply_policy(
    unit: &str,
    canonical: &std::path::Path,
    start: ServiceStart,
    manager: &mut dyn SystemdManager,
    changes: &mut Vec<UnitChange>,
) -> Result<(), ExecError> {
    let canonical_text = canonical.to_string_lossy().into_owned();
    match start {
        ServiceStart::Automatic => {
            let state = manager.unit_file_state(unit).map_err(ExecError::from)?;
            if state == "masked" || state == "masked-runtime" {
                changes.extend(manager.unmask(unit).map_err(ExecError::from)?);
            }
            changes.extend(manager.enable(unit).map_err(ExecError::from)?);
        }
        ServiceStart::Manual => {
            let state = manager.unit_file_state(unit).map_err(ExecError::from)?;
            if state == "masked" || state == "masked-runtime" {
                changes.extend(manager.unmask(unit).map_err(ExecError::from)?);
            }
            if state == "enabled" || state == "enabled-runtime" {
                changes.extend(
                    manager
                        .remove_owned_enablement(unit, &canonical_text)
                        .map_err(ExecError::from)?,
                );
            }
        }
        ServiceStart::Disabled => {
            let state = manager.unit_file_state(unit).map_err(ExecError::from)?;
            if state == "enabled" || state == "enabled-runtime" {
                changes.extend(
                    manager
                        .remove_owned_enablement(unit, &canonical_text)
                        .map_err(ExecError::from)?,
                );
            }
            changes.extend(manager.mask(unit).map_err(ExecError::from)?);
        }
    }
    Ok(())
}

pub(crate) fn refuse_foreign_integration(
    unit: &str,
    canonical: &std::path::Path,
) -> Result<(), ExecError> {
    let foreign = find_foreign_integration(
        unit,
        canonical,
        std::path::Path::new("/etc/systemd/system"),
        std::path::Path::new("/run/systemd/system"),
    );
    if foreign.is_empty() {
        return Ok(());
    }
    Err(ExecError::Ambiguous {
        unit: unit.to_owned(),
        reason: format!(
            "unrelated systemd integration exists, refusing to change enablement around it: {}",
            foreign
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

fn find_foreign_integration(
    unit: &str,
    canonical: &std::path::Path,
    etc_root: &std::path::Path,
    run_root: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    fn is_symlink(path: &std::path::Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    }

    fn is_real_dir(path: &std::path::Path) -> bool {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    }

    let canonical_text = canonical.to_string_lossy();
    let mut foreign = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut push = |path: std::path::PathBuf| {
        if seen.insert(path.clone()) {
            foreign.push(path);
        }
    };

    for root in [etc_root, run_root] {
        let Ok(layer) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in layer.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if !is_real_dir(&path) {
                if name == unit && is_symlink(&path) {
                    let target = std::fs::read_link(&path).unwrap_or_default();
                    if target.to_string_lossy() != canonical_text
                        && target.to_string_lossy() != "/dev/null"
                    {
                        push(path.clone());
                    }
                }

                if name != unit
                    && name.ends_with(".service")
                    && is_symlink(&path)
                    && std::fs::read_link(&path)
                        .is_ok_and(|target| target.to_string_lossy() == canonical_text)
                {
                    push(path.clone());
                }
                continue;
            }

            let stem = name
                .strip_suffix(".wants")
                .or_else(|| name.strip_suffix(".requires"));
            if stem.is_none() {
                continue;
            }
            let candidate = path.join(unit);
            if candidate == etc_root.join("multi-user.target.wants").join(unit) {
                continue;
            }
            if is_symlink(&candidate) {
                push(candidate);
            }
        }
    }
    foreign.sort();
    foreign
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn layer(base: &tempfile::TempDir, name: &str) -> PathBuf {
        let dir = base.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scan(
        base: &tempfile::TempDir,
        unit: &str,
        canonical: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        find_foreign_integration(
            unit,
            canonical,
            &base.path().join("etc"),
            &base.path().join("run"),
        )
    }

    const UNIT: &str = "zup-scan.service";
    const CANONICAL: &str = "/usr/local/lib/systemd/system/zup-scan.service";

    #[test]
    fn a_clean_tree_reports_nothing() {
        let base = tempfile::tempdir().unwrap();
        layer(&base, "etc");
        layer(&base, "run");
        let canonical = std::path::Path::new(CANONICAL);
        assert!(scan(&base, UNIT, canonical).is_empty());
    }

    #[test]
    fn the_owned_wants_link_is_excluded() {
        let base = tempfile::tempdir().unwrap();
        let etc = layer(&base, "etc");
        layer(&base, "run");
        let owned = etc.join("multi-user.target.wants");
        std::fs::create_dir_all(&owned).unwrap();
        std::os::unix::fs::symlink(CANONICAL, owned.join(UNIT)).unwrap();
        let canonical = std::path::Path::new(CANONICAL);
        assert!(scan(&base, UNIT, canonical).is_empty());
    }

    #[rstest::rstest]
    #[case::foreign_requires("etc", "some.target.requires", UNIT, CANONICAL)]
    #[case::foreign_wants("etc", "graphical.target.wants", UNIT, CANONICAL)]
    #[case::direct_alias("etc", "", "zup-scan-alias.service", CANONICAL)]
    #[case::runtime_run_link("run", "multi-user.target.wants", UNIT, CANONICAL)]
    #[case::same_name_elsewhere("etc", "", UNIT, "/usr/lib/systemd/system/zup-scan.service")]
    fn foreign_integration_is_named(
        #[case] tree: &str,
        #[case] sub: &str,
        #[case] name: &str,
        #[case] target: &str,
    ) {
        let base = tempfile::tempdir().unwrap();
        let root = layer(&base, tree);
        for other in ["etc", "run"] {
            if other != tree {
                layer(&base, other);
            }
        }
        let dir = if sub.is_empty() { root } else { root.join(sub) };
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join(name);
        std::os::unix::fs::symlink(target, &link).unwrap();
        let canonical = std::path::Path::new(CANONICAL);
        assert_eq!(scan(&base, UNIT, canonical), vec![link]);
    }

    #[test]
    fn a_mask_link_is_policy_not_integration() {
        let base = tempfile::tempdir().unwrap();
        let etc = layer(&base, "etc");
        layer(&base, "run");
        std::os::unix::fs::symlink("/dev/null", etc.join(UNIT)).unwrap();
        let canonical = std::path::Path::new(CANONICAL);
        assert!(scan(&base, UNIT, canonical).is_empty());
    }

    #[test]
    fn a_symlinked_wants_dir_is_never_descended() {
        let base = tempfile::tempdir().unwrap();
        let etc = layer(&base, "etc");
        layer(&base, "run");
        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(UNIT), b"[Unit]\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, etc.join("multi-user.target.wants")).unwrap();
        let canonical = std::path::Path::new(CANONICAL);
        assert!(
            scan(&base, UNIT, canonical).is_empty(),
            "a redirected wants directory is not traversed"
        );
    }
}

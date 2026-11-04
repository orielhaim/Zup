use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use zup_bootstrap::{
    BootstrapError, BootstrapOperation, BootstrapOutcome, BootstrapPlan, BootstrapState,
    DetectionResult, PrerequisiteProvider, PrerequisiteSatisfier, ProviderOutcome, ProviderRequest,
};
use zup_core::{PrerequisitePackage, PrerequisiteRequirement};
use zup_runtime::{BootstrapRequest, RuntimeControl, RuntimeEvent};

type Result<T> = std::result::Result<T, BootstrapError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Apt,
    Dnf,
    Apk,
    Pacman,
}

impl Manager {
    fn discover() -> Result<Self> {
        let release = std::fs::read_to_string("/etc/os-release")
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))?;
        if release.len() > 65536 {
            return Err(BootstrapError::Limit("os-release is too large"));
        }
        let identities = release
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect::<BTreeMap<_, _>>();
        let identities = ["ID", "ID_LIKE"]
            .into_iter()
            .filter_map(|key| identities.get(key))
            .map(|value| value.trim_matches(['\'', '"']))
            .collect::<Vec<_>>();
        for identity in identities
            .iter()
            .flat_map(|value| value.split_ascii_whitespace())
        {
            let manager = match identity {
                "debian" | "ubuntu" => Self::Apt,
                "fedora" | "rhel" | "centos" => Self::Dnf,
                "alpine" => Self::Apk,
                "arch" | "manjaro" => Self::Pacman,
                _ => continue,
            };
            if Path::new(manager.executable()).is_file() {
                return Ok(manager);
            }
        }
        Err(BootstrapError::ProviderPreflight(
            "no supported native package manager for this distribution".into(),
        ))
    }

    fn executable(self) -> &'static str {
        match self {
            Self::Apt => "/usr/bin/apt-get",
            Self::Dnf => "/usr/bin/dnf",
            Self::Apk => "/sbin/apk",
            Self::Pacman => "/usr/bin/pacman",
        }
    }

    fn installed(self, id: &str, minimum: Option<&str>) -> Result<bool> {
        match self {
            Self::Apt => {
                let output = query(
                    "/usr/bin/dpkg-query",
                    &[
                        "--show",
                        "--showformat=${db:Status-Status}\n${Version}",
                        "--",
                        id,
                    ],
                )?;
                if output.status.code() == Some(1) {
                    return Ok(false);
                }
                successful(&output)?;
                let text = String::from_utf8(output.stdout).map_err(|_| {
                    BootstrapError::Requirement("dpkg returned non-UTF-8 data".into())
                })?;
                let Some(version) = text.strip_prefix("installed\n") else {
                    return Ok(false);
                };
                match minimum {
                    None => Ok(true),
                    Some(minimum) => native_status(
                        query(
                            "/usr/bin/dpkg",
                            &["--compare-versions", version.trim(), "ge", minimum],
                        )?,
                        1,
                    ),
                }
            }
            Self::Dnf => {
                let output = query(
                    "/usr/bin/rpm",
                    &[
                        "--query",
                        "--queryformat",
                        "%{ARCH}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\n",
                        "--",
                        id,
                    ],
                )?;
                if output.status.code() == Some(1) && output.stderr.is_empty() {
                    return Ok(false);
                }
                successful(&output)?;
                let architecture = query("/usr/bin/rpm", &["--eval", "%{_arch}"])?;
                successful(&architecture)?;
                let architecture = String::from_utf8(architecture.stdout).map_err(|_| {
                    BootstrapError::Requirement("RPM returned a non-UTF-8 architecture".into())
                })?;
                let versions = String::from_utf8(output.stdout).map_err(|_| {
                    BootstrapError::Requirement("RPM returned non-UTF-8 versions".into())
                })?;
                for record in versions.lines() {
                    let (installed_arch, version) = record.split_once('\t').ok_or_else(|| {
                        BootstrapError::Requirement("RPM returned an invalid package record".into())
                    })?;
                    if installed_arch != "noarch" && installed_arch != architecture.trim() {
                        continue;
                    }
                    let Some(minimum) = minimum else {
                        return Ok(true);
                    };
                    let expression = rpm_version_expression(version, minimum)?;
                    let comparison = query("/usr/bin/rpm", &["--eval", &expression])?;
                    successful(&comparison)?;
                    match comparison.stdout.as_slice() {
                        b"1\n" => return Ok(true),
                        b"0\n" => {}
                        _ => {
                            return Err(BootstrapError::ProviderPreflight(
                                "native RPM version comparison requires RPM 4.16 or newer".into(),
                            ));
                        }
                    }
                }
                Ok(false)
            }
            Self::Apk => {
                let dependency = dependency(id, minimum);
                native_status(
                    query(self.executable(), &["info", "--exists", "--", &dependency])?,
                    1,
                )
            }
            Self::Pacman => {
                let dependency = dependency(id, minimum);
                native_status(
                    query(self.executable(), &["--deptest", "--", &dependency])?,
                    127,
                )
            }
        }
    }

    fn install_arguments(self, id: &str, minimum: Option<&str>) -> Vec<String> {
        let (arguments, package) = match self {
            Self::Apt => (
                vec![
                    "--assume-yes",
                    "--no-remove",
                    "-o",
                    "DPkg::Lock::Timeout=0",
                    "install",
                    "--",
                ],
                id.to_owned(),
            ),
            Self::Dnf => (
                vec!["--assumeyes", "--setopt=exit_on_lock=True", "install", "--"],
                minimum.map_or_else(|| id.to_owned(), |version| format!("{id} >= {version}")),
            ),
            Self::Apk => (vec!["add", "--"], dependency(id, minimum)),
            Self::Pacman => (
                vec!["--sync", "--needed", "--noconfirm", "--"],
                id.to_owned(),
            ),
        };
        arguments
            .into_iter()
            .map(str::to_owned)
            .chain([package])
            .collect()
    }
}

fn dependency(id: &str, minimum: Option<&str>) -> String {
    minimum.map_or_else(|| id.to_owned(), |version| format!("{id}>={version}"))
}

fn rpm_version_expression(found: &str, required: &str) -> Result<String> {
    if [found, required].iter().any(|version| {
        version.is_empty()
            || version.len() > 256
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".:+~^_-".contains(&byte))
    }) {
        return Err(BootstrapError::Requirement(
            "RPM package versions contain unsupported characters".into(),
        ));
    }
    Ok(format!("%[v\"{found}\" >= v\"{required}\"]"))
}

fn query(executable: &str, arguments: &[&str]) -> Result<std::process::Output> {
    Command::new(executable)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(BootstrapError::Io)
}

fn successful(output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(BootstrapError::Provider(format!(
            "package manager exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn native_status(output: std::process::Output, absent: i32) -> Result<bool> {
    if output.status.code() == Some(absent) && (absent != 1 || output.stderr.is_empty()) {
        return Ok(false);
    }
    successful(&output)?;
    Ok(true)
}

pub(crate) fn validate(
    requirement: &PrerequisiteRequirement,
    package: &PrerequisitePackage,
) -> Result<()> {
    let PrerequisiteRequirement::NativePackage { .. } = requirement else {
        return Err(BootstrapError::Requirement(
            "Linux prerequisites require kind = native_package".into(),
        ));
    };
    requirement
        .validate_native_package()
        .map_err(|reason| BootstrapError::Requirement(reason.into()))?;
    if !matches!(
        package,
        PrerequisitePackage::DetectOnly | PrerequisitePackage::Repository
    ) {
        return Err(BootstrapError::ProviderPreflight("Linux prerequisite artifacts are not supported; use a repository or detect-only requirement".into()));
    }
    Ok(())
}

struct NativeProvider<'a> {
    manager: Manager,
    control: &'a RuntimeControl,
}

impl PrerequisiteSatisfier for NativeProvider<'_> {
    fn satisfy(&self, operation: &BootstrapOperation) -> Result<DetectionResult> {
        validate(&operation.requirement, &operation.package)?;
        if !matches!(
            operation.target,
            zup_core::PrerequisiteArchitecture::Current | zup_core::PrerequisiteArchitecture::Any
        ) {
            return Err(BootstrapError::Requirement("Linux native packages use the package manager's native architecture; use target = current".into()));
        }
        let PrerequisiteRequirement::NativePackage {
            id,
            minimum_version,
        } = &operation.requirement
        else {
            unreachable!()
        };
        let satisfied = self
            .manager
            .installed(id.as_str(), minimum_version.as_deref())?;
        let _ = self.control.events.send(RuntimeEvent::PrerequisiteCheck {
            id: operation.id.to_string(),
            name: operation.name.clone(),
            satisfied,
            version: None,
        });
        Ok(if satisfied {
            DetectionResult::Satisfied {
                version: None,
                evidence: format!(
                    "{:?}: {}",
                    self.manager,
                    dependency(id.as_str(), minimum_version.as_deref())
                ),
            }
        } else {
            DetectionResult::Missing
        })
    }
}

impl PrerequisiteProvider for NativeProvider<'_> {
    fn execute(&self, request: &ProviderRequest) -> Result<ProviderOutcome> {
        validate(&request.requirement, &request.package)?;
        if self.control.cancellation.is_cancelled() {
            return Err(BootstrapError::ProviderPreflight(
                "prerequisite installation cancelled before launch".into(),
            ));
        }
        if !matches!(request.package, PrerequisitePackage::Repository)
            || !request.arguments.is_empty()
        {
            return Err(BootstrapError::ProviderPreflight(
                "native package installation requires repository policy and no installer arguments"
                    .into(),
            ));
        }
        let PrerequisiteRequirement::NativePackage {
            id,
            minimum_version,
        } = &request.requirement
        else {
            unreachable!()
        };
        let root = rustix::process::geteuid().is_root();
        if !root && !self.control.policy.allows_authorization() {
            return Err(BootstrapError::ProviderPreflight(
                "system package installation needs root or interactive polkit authorization".into(),
            ));
        }
        let mut command = Command::new(if root {
            self.manager.executable()
        } else {
            "/usr/bin/pkexec"
        });
        if !root {
            command.arg(self.manager.executable());
        }
        command
            .args(
                self.manager
                    .install_arguments(id.as_str(), minimum_version.as_deref()),
            )
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LC_ALL", "C")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null());
        let _ = self.control.events.send(RuntimeEvent::StateChanged {
            state: zup_runtime::RuntimeState::InstallingPrerequisites,
        });
        let _ = self.control.events.send(RuntimeEvent::PrerequisiteInstall {
            id: request.prerequisite_id.to_string(),
            name: id.to_string(),
        });
        let status = command.status()?;
        if !status.success() {
            return Err(BootstrapError::Provider(format!(
                "native package installation exited with {status}; check package-manager diagnostics and repair its database before retrying"
            )));
        }
        Ok(ProviderOutcome::Succeeded)
    }
}

pub(crate) fn prepare(
    install: &zup_plan::DesiredState,
) -> std::result::Result<Option<BootstrapRequest>, String> {
    if install.prerequisites.is_empty() {
        return Ok(None);
    }
    let operations = install
        .prerequisites
        .iter()
        .map(|prerequisite| {
            validate(&prerequisite.requirement, &prerequisite.package)?;
            if prerequisite.installer != zup_core::PrerequisiteInstaller::default() {
                return Err(BootstrapError::ProviderPreflight(
                    "native package prerequisites do not accept artifact installer options".into(),
                ));
            }
            Ok(BootstrapOperation {
                id: prerequisite.id.clone(),
                name: prerequisite.name.to_string(),
                target: prerequisite.target,
                requirement: prerequisite.requirement.clone(),
                package: prerequisite.package.clone(),
                installer: prerequisite.installer.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    let plan = BootstrapPlan::new(
        zup_bootstrap::BootstrapKey {
            app_id: install.app.id.clone(),
            app_version: install.app.version.clone(),
            target: install.target,
            scope: install.scope,
        },
        operations,
    )
    .map_err(|error| error.to_string())?;
    let plan = zup_bootstrap::BoundBootstrapPlan::new(plan, BTreeMap::new())
        .map_err(|error| error.to_string())?;
    let state_root = crate::paths::user_state_root().map_err(|error| error.to_string())?;
    Ok(Some(BootstrapRequest {
        plan,
        quarantine_root: state_root.join("bootstrap"),
        state_root,
    }))
}

pub(crate) fn execute(
    request: &BootstrapRequest,
    control: &RuntimeControl,
) -> std::result::Result<(), String> {
    execute_inner(request, control).map_err(|error| error.to_string())
}

fn execute_inner(request: &BootstrapRequest, control: &RuntimeControl) -> Result<()> {
    let _ = control.events.send(RuntimeEvent::StateChanged {
        state: zup_runtime::RuntimeState::CheckingPrerequisites,
    });
    let plan = &request.plan.plan;
    let lock_key = format!("bootstrap-{}", request.plan.id);
    let _lock = zup_transaction::InstallationLock::try_acquire(&request.state_root, &lock_key)
        .map_err(|error| BootstrapError::Provider(error.to_string()))?
        .ok_or_else(|| {
            BootstrapError::ProviderPreflight("prerequisite bootstrap is busy".into())
        })?;
    let root = private_fs::PrivateDir::open(&request.state_root)
        .map_err(|error| BootstrapError::Provider(error.to_string()))?;
    let directory = root
        .ensure_dir("bootstrap")
        .and_then(|directory| directory.ensure_dir(request.plan.id.to_string()))
        .map_err(|error| BootstrapError::Provider(error.to_string()))?;
    let mut state = match directory.open_file("state.json") {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(zup_bootstrap::MAX_BOOTSTRAP_STATE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > zup_bootstrap::MAX_BOOTSTRAP_STATE_BYTES {
                return Err(BootstrapError::Limit("bootstrap state too large"));
            }
            serde_json::from_slice::<BootstrapState>(&bytes)?
        }
        Err(private_fs::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            BootstrapState::new(plan)
        }
        Err(error) => return Err(BootstrapError::Provider(error.to_string())),
    };
    state.validate(plan)?;
    let provider = NativeProvider {
        manager: Manager::discover()?,
        control,
    };
    let mut persist = |state: &BootstrapState| {
        let bytes = serde_json::to_vec(state)?;
        zup_transaction::publish_full(
            &directory,
            "state.json",
            &bytes,
            private_fs::Publication::Replace,
        )
        .map_err(|error| BootstrapError::Provider(error.to_string()))
    };
    persist(&state)?;
    if zup_bootstrap::recover(plan, &provider, &mut state)? == BootstrapOutcome::RecoveryRequired {
        persist(&state)?;
        return Err(BootstrapError::RecoveryRequired("an interrupted package operation is not satisfied; repair it using the native package manager".into()));
    }
    persist(&state)?;
    zup_bootstrap::execute_plan_with_persist(
        plan,
        &provider,
        &provider,
        |_| Ok(PathBuf::new()),
        &mut state,
        &mut persist,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_debian_versions_use_dpkg_ordering() {
        if !Path::new("/usr/bin/dpkg-query").is_file() {
            return;
        }
        assert!(Manager::Apt.installed("dpkg", Some("0:0~rc1-1")).unwrap());
        assert!(!Manager::Apt.installed("dpkg", Some("999:1-1")).unwrap());
        assert!(
            !Manager::Apt
                .installed("zup-nonexistent-package-98ad3297", None)
                .unwrap()
        );
    }

    #[rstest::rstest]
    #[case("--config", None)]
    #[case("pkg*", None)]
    #[case("valid", Some("1; touch /tmp/injected"))]
    #[case("valid", Some(""))]
    fn package_selectors_cannot_be_options_or_expressions(
        #[case] id: &str,
        #[case] version: Option<&str>,
    ) {
        let requirement = PrerequisiteRequirement::NativePackage {
            id: zup_core::InstalledPackageId::new(id).unwrap(),
            minimum_version: version.map(str::to_owned),
        };
        assert!(validate(&requirement, &PrerequisitePackage::Repository).is_err());
    }

    #[test]
    fn rpm_comparisons_accept_versions_and_reject_macro_injection() {
        assert_eq!(
            rpm_version_expression("2:1.0~rc1-4.fc44", "1:9.0").unwrap(),
            "%[v\"2:1.0~rc1-4.fc44\" >= v\"1:9.0\"]"
        );
        assert!(rpm_version_expression("%{lua:os.exit()}", "1").is_err());
        assert!(rpm_version_expression("1", "1\" || 1").is_err());
    }

    #[test]
    fn package_database_errors_are_not_missing_packages() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"database is unreadable".to_vec(),
        };
        assert!(native_status(output, 1).is_err());
    }

    #[test]
    fn every_install_uses_a_fixed_operation_and_an_argument_boundary() {
        for manager in [Manager::Apt, Manager::Dnf, Manager::Apk, Manager::Pacman] {
            let arguments = manager.install_arguments("example", Some("2:1.0-3"));
            assert_eq!(arguments[arguments.len() - 2], "--");
            assert!(
                !arguments
                    .iter()
                    .any(|argument| argument == "update" || argument == "--allow-untrusted")
            );
        }
    }
}

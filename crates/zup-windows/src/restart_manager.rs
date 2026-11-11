use std::path::{Path, PathBuf};

use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
use windows::core::PCWSTR;

use crate::lowering::host_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingProcess {
    pub service_name: Option<String>,
    pub execution_generation: String,
    pub pid: u32,
    pub name: String,
    pub app_type: i32,
    pub restartable: bool,
}

pub(crate) fn preflight_services(
    files: &[&Path],
    controls: &[zup_plan::ServiceControlOperation],
    lifecycle: &mut crate::service_control::ScmLifecycle,
) -> Result<FilePreflight, String> {
    use zup_transaction::NativeServiceManager;
    for operation in controls
        .iter()
        .filter(|operation| operation.stage == zup_plan::ServiceControlStage::Quiesce)
    {
        lifecycle.observe(operation)?;
    }
    let FilePreflight::Blocked {
        mut processes,
        reboot_reason,
    } = preflight(files)?
    else {
        return Ok(FilePreflight::Ready);
    };
    for operation in controls
        .iter()
        .filter(|operation| operation.stage == zup_plan::ServiceControlStage::Quiesce)
    {
        let observed = lifecycle.observe(operation)?;
        if observed.configuration != operation.before_configuration
            || observed.manager_generation != operation.manager_generation
            || observed.execution_generation != operation.before_execution_generation
            || !operation.before.matches(observed.runtime)
        {
            return Err("service changed during executable preflight".into());
        }
        processes.retain(|process| {
            process.service_name.as_deref() != Some(operation.native_name.as_str())
                || process.execution_generation != operation.before_execution_generation
        });
    }
    Ok(if processes.is_empty() && reboot_reason == 0 {
        FilePreflight::Ready
    } else {
        FilePreflight::Blocked {
            processes,
            reboot_reason,
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePreflight {
    Ready,
    Blocked {
        processes: Vec<BlockingProcess>,
        reboot_reason: u32,
    },
}

pub fn preflight(files: &[&Path]) -> Result<FilePreflight, String> {
    if files.is_empty() {
        return Ok(FilePreflight::Ready);
    }

    unsafe {
        let mut session: u32 = 0;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        let rc = RmStartSession(&mut session, None, windows::core::PWSTR(key.as_mut_ptr()));
        if rc.0 != 0 {
            return Err(format!("RmStartSession failed ({})", rc.0));
        }
        let _session = RestartManagerSession(session);

        let wides: Vec<Vec<u16>> = files
            .iter()
            .map(|p| {
                p.display()
                    .to_string()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect()
            })
            .collect();
        let ptrs: Vec<PCWSTR> = wides.iter().map(|w| PCWSTR(w.as_ptr())).collect();

        let rc = RmRegisterResources(session, Some(&ptrs), None, None);
        if rc.0 != 0 {
            return Err(format!("RmRegisterResources failed ({})", rc.0));
        }

        let mut needed: u32 = 0;
        let mut count: u32 = 0;
        let mut reboot: u32 = 0;
        let rc = RmGetList(session, &mut needed, &mut count, None, &mut reboot);
        if rc.0 == 234 {
            let mut capacity = (needed as usize).max(1);
            for _ in 0..3 {
                let mut processes = vec![RM_PROCESS_INFO::default(); capacity];
                let mut needed = 0;
                let mut count = processes.len() as u32;
                let mut reboot = 0;
                let rc = RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    Some(processes.as_mut_ptr()),
                    &mut reboot,
                );
                if rc.0 == 0 {
                    processes.truncate(count.min(processes.len() as u32) as usize);
                    return Ok(blocked_or_ready(processes, reboot));
                }
                if rc.0 != 234 {
                    return Err(format!("RmGetList failed ({})", rc.0));
                }
                capacity = capacity.saturating_mul(2).max(needed as usize).max(1);
            }
            return Err("RmGetList remained more-data after three attempts".into());
        }
        if rc.0 != 0 {
            return Err(format!("RmGetList failed ({})", rc.0));
        }

        Ok(blocked_or_ready(Vec::new(), reboot))
    }
}

pub fn preflight_paths(plan: &zup_plan::DeploymentPlan) -> Vec<PathBuf> {
    plan.nodes
        .iter()
        .filter(|node| {
            matches!(
                node.kind,
                zup_plan::NodeKind::FileMutation {
                    delta: zup_plan::FileOperationKind::Replace
                        | zup_plan::FileOperationKind::RepairOwned,
                    ..
                } | zup_plan::NodeKind::FileRemoval { .. }
            )
        })
        .filter_map(|node| node.file_destination().map(host_path))
        .collect()
}

pub fn blocked_reason(blocked: &FilePreflight) -> Option<String> {
    let FilePreflight::Blocked {
        processes,
        reboot_reason,
    } = blocked
    else {
        return None;
    };
    let mut detail = processes
        .iter()
        .map(|process| format!("{} (PID {})", process.name, process.pid))
        .collect::<Vec<_>>();
    if detail.is_empty() {
        detail.push(format!(
            "resource preflight requested a restart: {reboot_reason}"
        ));
    }
    Some(detail.join("\n"))
}

struct RestartManagerSession(u32);
impl Drop for RestartManagerSession {
    fn drop(&mut self) {
        unsafe {
            let _ = RmEndSession(self.0);
        }
    }
}

fn blocked_or_ready(processes: Vec<RM_PROCESS_INFO>, reboot_reason: u32) -> FilePreflight {
    let processes = processes
        .into_iter()
        .map(|process| {
            let name_end = process
                .strAppName
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(process.strAppName.len());
            let name = String::from_utf16_lossy(&process.strAppName[..name_end]);
            let name = if name.is_empty() {
                format!("Process {}", process.Process.dwProcessId)
            } else {
                name
            };
            BlockingProcess {
                service_name: {
                    let end = process
                        .strServiceShortName
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(process.strServiceShortName.len());
                    (end != 0)
                        .then(|| String::from_utf16_lossy(&process.strServiceShortName[..end]))
                },
                execution_generation: format!(
                    "{}:{}:{}",
                    process.Process.dwProcessId,
                    process.Process.ProcessStartTime.dwHighDateTime,
                    process.Process.ProcessStartTime.dwLowDateTime
                ),
                pid: process.Process.dwProcessId,
                name,
                app_type: process.ApplicationType.0,
                restartable: process.bRestartable.as_bool(),
            }
        })
        .collect::<Vec<_>>();
    if processes.is_empty() && reboot_reason == 0 {
        FilePreflight::Ready
    } else {
        FilePreflight::Blocked {
            processes,
            reboot_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zup_core::{
        InstallationId, Privilege, RelativePath, ResourceKey, Target, TargetPath, hash_bytes,
    };
    use zup_plan::{
        DeploymentEffects, FileOperation, FileOperationKind, FilePrecondition, compile_deployment,
    };

    #[rstest::rstest]
    #[case::create(FileOperationKind::Create, false)]
    #[case::restore_missing(FileOperationKind::RestoreOwned, false)]
    #[case::retain(FileOperationKind::NoOp, false)]
    #[case::replace(FileOperationKind::Replace, true)]
    #[case::repair(FileOperationKind::RepairOwned, true)]
    fn preflight_queries_only_existing_objects_that_will_change(
        #[case] kind: FileOperationKind,
        #[case] queried: bool,
    ) {
        let directory = TargetPath::new(Target::WINDOWS_X64, r"C:\Application").unwrap();
        let destination = directory.join("program.exe").unwrap();
        let mut effects = DeploymentEffects::new(
            InstallationId::new_v7(),
            crate::requirements_fixture::requirements(
                Target::WINDOWS_X64,
                zup_core::SelectedScope::User,
                "example.application",
                "1.0.0",
            ),
        );
        effects.install_directory = Some(directory);
        effects.files.push(FileOperation {
            key: ResourceKey::File {
                id: zup_core::FileResourceId::Payload {
                    id: zup_core::PayloadId::File {
                        declaration: zup_core::ResourceId::new("fixture-0").unwrap(),
                    },
                },
            },
            source_relative: RelativePath::new("program.exe").unwrap(),
            destination: destination.clone(),
            precondition: if matches!(
                kind,
                FileOperationKind::Create | FileOperationKind::RestoreOwned
            ) {
                FilePrecondition::Absent
            } else {
                FilePrecondition::Exact {
                    object: zup_core::NativeObjectIdentity::Windows {
                        volume: 1,
                        file: [1; 16],
                    },
                    size: 3,
                    sha256: hash_bytes(b"old"),
                }
            },
            expected_sha256: hash_bytes(b"old"),
            expected_size: 3,
            privilege: Privilege::User,
            kind,
            executable: true,
            conflict: None,
            replacement: zup_core::FixedReplacement::Atomic,
        });
        let paths = preflight_paths(&compile_deployment(&effects).unwrap());
        assert_eq!(
            paths,
            if queried {
                vec![host_path(&destination)]
            } else {
                Vec::new()
            }
        );
    }
}

//! Restart Manager blocker discovery (read-only).

use std::path::{Path, PathBuf};

use windows::Win32::System::RestartManager::{
    RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};
use windows::core::PCWSTR;
use zup_core::TargetTriple;
use zup_exec::FileOperation;
use zup_platform::TargetPath;

use crate::lowering::host_path;

/// A process/service blocking a target resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingProcess {
    pub pid: u32,
    pub name: String,
    pub app_type: i32,
    pub restartable: bool,
}

/// Preflight result before crossing commit intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePreflight {
    Ready,
    Blocked {
        processes: Vec<BlockingProcess>,
        reboot_reason: u32,
    },
}

/// Discover processes locking files this transaction may mutate.
///
/// Read-only: never calls `RmShutdown` / `RmRestart`.
pub fn preflight(files: &[&Path]) -> Result<FilePreflight, String> {
    if files.is_empty() {
        return Ok(FilePreflight::Ready);
    }

    unsafe {
        let mut session: u32 = 0;
        let mut key = [0u16; 32];
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
                let mut count = 0;
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

/// Files a transaction plan will mutate, as host paths.
///
/// The preflight before a transaction starts and the barrier preflight
/// immediately before commit intent read this one set, so the two can never
/// disagree about what is at risk.
pub fn plan_mutating_paths(
    plan: &zup_transaction::TransactionPlan,
    target: &TargetTriple,
) -> Vec<PathBuf> {
    plan.nodes
        .iter()
        .filter_map(|node| match &node.kind {
            zup_transaction::NodeKind::FileMutation {
                key,
                delta:
                    zup_transaction::FileDelta::Create
                    | zup_transaction::FileDelta::Replace
                    | zup_transaction::FileDelta::RestoreOwned
                    | zup_transaction::FileDelta::RepairOwned,
            } => {
                let destination = match key {
                    zup_core::ResourceKey::File { destination }
                    | zup_core::ResourceKey::Maintenance { destination, .. } => destination,
                    _ => return None,
                };
                let path = TargetPath::new(target.clone(), destination).ok()?;
                Some(host_path(&path))
            }
            zup_transaction::NodeKind::FileRemoval { .. } => node
                .meta
                .removal
                .as_ref()
                .map(|removal| host_path(&removal.destination)),
            _ => None,
        })
        .collect()
}

/// Why a preflight is blocked, one line per blocker.
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

/// Files that a plan will actually mutate (Create/Replace only).
pub fn mutating_paths(files: &[FileOperation]) -> Vec<std::path::PathBuf> {
    files
        .iter()
        .filter(|f| {
            matches!(
                f.kind,
                zup_exec::FileOperationKind::Create | zup_exec::FileOperationKind::Replace
            )
        })
        .map(|f| host_path(&f.destination))
        .collect()
}

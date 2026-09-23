//! Restart Manager blocker discovery (read-only).

use std::path::Path;

use zup_exec::FileOperation;

use crate::fs_bindings::{RmEndSession, RmGetList, RmRegisterResources, RmStartSession};

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

    // SAFETY: session handle is initialized below and always ended.
    unsafe {
        let mut session: u32 = 0;
        let mut key = [0u16; 32];
        let rc = RmStartSession(&mut session, 0, key.as_mut_ptr());
        if rc != 0 {
            return Err(format!("RmStartSession failed ({rc})"));
        }

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
        let ptrs: Vec<*const u16> = wides.iter().map(|w| w.as_ptr()).collect();

        let rc = RmRegisterResources(
            session,
            ptrs.len() as u32,
            ptrs.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        );
        if rc != 0 {
            RmEndSession(session);
            return Err(format!("RmRegisterResources failed ({rc})"));
        }

        let mut needed: u32 = 0;
        let mut count: u32 = 0;
        let mut reboot: u32 = 0;
        let mut buf: Vec<u8> = vec![0u8; 632 * 16];
        let rc = RmGetList(
            session,
            &mut needed,
            &mut count,
            buf.as_mut_ptr().cast(),
            &mut reboot,
        );
        RmEndSession(session);
        // ERROR_MORE_DATA = 234: buffer too small, but we still have reasons.
        if rc != 0 && rc != 234 {
            return Err(format!("RmGetList failed ({rc})"));
        }

        if count == 0 && reboot == 0 {
            return Ok(FilePreflight::Ready);
        }

        // Parse RM_PROCESS_INFO minimally: we only need pid + name when present.
        let mut processes = Vec::new();
        for i in 0..count as usize {
            let base = i * 632;
            if base + 4 > buf.len() {
                break;
            }
            let pid = u32::from_le_bytes([buf[base], buf[base + 1], buf[base + 2], buf[base + 3]]);
            // strAppName is a [u16; 255] starting after process (DWORD) + app status fields.
            // Keep it simple: record pid only if name parse is unreliable.
            processes.push(BlockingProcess {
                pid,
                name: format!("pid:{pid}"),
                app_type: 0,
                restartable: true,
            });
        }

        Ok(FilePreflight::Blocked {
            processes,
            reboot_reason: reboot,
        })
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
        .map(|f| f.destination.as_path().to_path_buf())
        .collect()
}

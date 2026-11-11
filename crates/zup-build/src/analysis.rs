use std::io::Read;
use zup_core::{PayloadPlacement, RelativePath, TargetBuildPlan};

#[derive(Debug)]
pub struct PayloadBinary {
    pub source: RelativePath,
    pub executable: zup_binary::Executable,
    pub target_conflict: Option<zup_binary::TargetRefusal>,
}

#[derive(Debug, Default)]
pub struct PayloadAnalysis {
    pub binaries: Vec<PayloadBinary>,
    pub incomplete: Vec<(RelativePath, String)>,
    pub requires_relocation_contract: bool,
}

pub fn analyze_payload(plan: &TargetBuildPlan) -> PayloadAnalysis {
    let mut analysis = PayloadAnalysis::default();
    for file in &plan.files {
        if file.size > 64 * 1024 * 1024 {
            if file.executable {
                analysis.incomplete.push((
                    file.source_relative.clone(),
                    "binary exceeds the 64 MiB static inspection limit".into(),
                ));
            }
            continue;
        }
        analysis.requires_relocation_contract |=
            file.executable && matches!(file.placement, PayloadPlacement::Generation { .. });
        let bytes = std::fs::File::open(&file.source).and_then(|mut input| {
            let mut bytes = Vec::new();
            input.by_ref().take(4).read_to_end(&mut bytes)?;
            if !bytes.starts_with(b"MZ") && !bytes.starts_with(b"\x7fELF") {
                return Ok(None);
            }
            input
                .take(64 * 1024 * 1024 + 1 - bytes.len() as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 * 1024 {
                return Err(std::io::Error::other(
                    "binary grew beyond the static inspection limit",
                ));
            }
            Ok(Some(bytes))
        });
        let inspected = match bytes {
            Ok(None) => continue,
            Ok(Some(bytes)) => zup_binary::Executable::from_bytes(&bytes),
            Err(error) => Err(zup_binary::InspectError::Io(error)),
        };
        match inspected {
            Ok(executable) => {
                analysis.requires_relocation_contract |=
                    matches!(file.placement, PayloadPlacement::Generation { .. });
                let target_conflict = executable.refuse_target(&plan.installer.target).err();
                analysis.binaries.push(PayloadBinary {
                    source: file.source_relative.clone(),
                    executable,
                    target_conflict,
                });
            }
            Err(zup_binary::InspectError::NotAnExecutable(_)) => {}
            Err(error) => analysis
                .incomplete
                .push((file.source_relative.clone(), error.to_string())),
        }
    }
    analysis
}

use serde::{Deserialize, Serialize};
use zup_core::{InstallationId, NativeObjectIdentity, TransactionId};
use zup_plan::{
    OwnedPersistentDirectory, PersistentBinding, PersistentDirectoryOperation,
    PersistentDirectoryStage, PersistentOwnership,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentPreparation {
    pub transaction: TransactionId,
    pub installation: InstallationId,
    pub operation: PersistentDirectoryOperation,
    pub staging: String,
    pub creation: PersistentCreation,
    pub access: PersistentAccessState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentAccessState {
    NotRequested,
    Requested,
    Verified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentDirectoryReceipt {
    pub stage: PersistentDirectoryStage,
    pub owned: OwnedPersistentDirectory,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PersistentCreationToken(uuid::Uuid);

impl PersistentCreationToken {
    pub fn bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}
impl std::fmt::Debug for PersistentCreationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PersistentCreationToken([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentCreation {
    NativeClaim {
        token: PersistentCreationToken,
    },
    Requested {
        token: PersistentCreationToken,
    },
    Created {
        token: PersistentCreationToken,
        object: NativeObjectIdentity,
    },
}
impl PersistentCreation {
    fn object(&self) -> Option<NativeObjectIdentity> {
        match self {
            Self::Created { object, .. } => Some(*object),
            Self::Requested { .. } | Self::NativeClaim { .. } => None,
        }
    }
    fn token(&self) -> &PersistentCreationToken {
        match self {
            Self::Requested { token }
            | Self::Created { token, .. }
            | Self::NativeClaim { token } => token,
        }
    }
}

pub trait PersistentFilesystem {
    type Directory;
    type Error;
    fn refused(reason: &str) -> Self::Error;
    fn recovery_required(record: &crate::TransactionRecord) -> Self::Error {
        Self::refused(&format!(
            "Transaction {} requires recovery: {}",
            record.transaction_id,
            record.recovery_detail()
        ))
    }
    fn parent(
        &self,
        operation: &PersistentDirectoryOperation,
    ) -> Result<Self::Directory, Self::Error>;
    fn open(
        &self,
        parent: &Self::Directory,
        name: &str,
    ) -> Result<Option<Self::Directory>, Self::Error>;
    fn creation_name(
        operation: &PersistentDirectoryOperation,
        transaction: TransactionId,
    ) -> String {
        format!(
            ".zup-data-{}-{}",
            transaction,
            zup_core::hash_bytes(operation.intent.directory.id.as_str().as_bytes()).to_hex()
        )
    }
    fn create(
        &self,
        parent: &Self::Directory,
        name: &str,
        token: &PersistentCreationToken,
    ) -> Result<Self::Directory, Self::Error>;
    fn authenticate_creation(
        &self,
        _directory: &Self::Directory,
        _token: &PersistentCreationToken,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
    fn object(directory: &Self::Directory) -> Result<NativeObjectIdentity, Self::Error>;
    fn publish(
        &self,
        parent: &Self::Directory,
        from: &str,
        to: &str,
        object: NativeObjectIdentity,
    ) -> Result<(), Self::Error>;
    fn load(
        &self,
        transaction: TransactionId,
        operation: &PersistentDirectoryOperation,
    ) -> Result<Option<PersistentPreparation>, Self::Error>;
    fn checkpoint(
        &self,
        record: &PersistentPreparation,
        previous: Option<&PersistentPreparation>,
    ) -> Result<(), Self::Error>;
    fn native_state_available(
        &self,
        _binding: &zup_plan::SystemdStateBinding,
    ) -> Result<(), Self::Error> {
        Err(Self::refused("this platform cannot reserve systemd state"))
    }
    fn verify_native_state(
        &self,
        _binding: &zup_plan::SystemdStateBinding,
    ) -> Result<(), Self::Error> {
        Err(Self::refused("this platform cannot verify systemd state"))
    }
    fn provision_access(
        &self,
        directory: &Self::Directory,
        binding: &zup_plan::FilesystemDataBinding,
    ) -> Result<(), Self::Error>;
    fn verify_access(
        &self,
        directory: &Self::Directory,
        binding: &zup_plan::FilesystemDataBinding,
    ) -> Result<(), Self::Error>;
    fn origin(&self, transaction: TransactionId) -> Result<crate::TransactionRecord, Self::Error>;
}

pub struct PersistentStorage<F>(pub F);

impl<F: PersistentFilesystem> PersistentStorage<F> {
    pub fn discover_claims(
        &self,
        store: &crate::FilesystemTransactionStore,
        app: &zup_core::AppId,
        scope: zup_core::SelectedScope,
        target: zup_core::Target,
        previous: Option<&zup_plan::InstallationRecord>,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, F::Error> {
        let records =
            crate::settled_records(store, app, scope, previous).map_err(|error| match error {
                crate::ReconciliationError::RecoveryRequired(transaction) => {
                    match crate::TransactionStore::load(store, &transaction) {
                        Ok(record) => F::recovery_required(&record),
                        Err(error) => F::refused(&error.to_string()),
                    }
                }
                error => F::refused(&error.to_string()),
            })?;
        let mut selected: Option<zup_plan::PersistentClaimSelection> = None;
        for record in records {
            if record.phase != crate::TransactionPhase::RolledBack
                || record.target != target
                || previous.is_some_and(|previous| previous.id != record.plan.installation)
            {
                continue;
            }
            let Some(reservation) = &record.reservation else {
                continue;
            };
            if !reservation.matches(&record) {
                return Err(F::refused(
                    "persistent claim has an invalid installation reservation",
                ));
            }
            for node in &record.plan.nodes {
                let zup_plan::NodeKind::PersistentDirectory { operation } = &node.kind else {
                    continue;
                };
                if operation.stage != PersistentDirectoryStage::Prepare
                    || operation.intent.previous.is_some()
                    || self.0.load(record.transaction_id, operation)?.is_none()
                {
                    continue;
                }
                let crate::ReconcileResult::AppliedWithReceipt(
                    crate::OperationReceipt::PersistentDirectory(receipt),
                ) = self.reconcile(operation, &record)?
                else {
                    return Err(F::refused(
                        "persistent claim publication is unresolved; recover its origin transaction first",
                    ));
                };
                let owned = receipt.owned;
                if let Some(committed) =
                    previous.and_then(|record| record.data.get(&owned.directory.id))
                {
                    if committed.ownership != owned.ownership
                        || committed.origin != owned.origin
                        || committed.directory.location != owned.directory.location
                    {
                        return Err(F::refused(
                            "failed-installation data claim conflicts with committed ownership",
                        ));
                    }
                    continue;
                }
                let selection =
                    selected.get_or_insert_with(|| zup_plan::PersistentClaimSelection {
                        installation: record.plan.installation,
                        directory: reservation.directory.clone(),
                        owners: Default::default(),
                    });
                if selection.installation != record.plan.installation
                    || !selection.directory.equivalent(&reservation.directory)
                {
                    return Err(F::refused(
                        "multiple failed installations retain application data; select and recover the original installation explicitly",
                    ));
                }
                if let Some(existing) = selection.owners.get(&owned.directory.id) {
                    if existing != &owned {
                        return Err(F::refused(
                            "persistent claims disagree about a resource's physical owner",
                        ));
                    }
                } else {
                    if selection.owners.values().try_fold(
                        false,
                        |overlap, existing| -> Result<bool, F::Error> {
                            Ok(overlap
                                || (existing
                                    .ownership
                                    .binding()
                                    .path(target)
                                    .map_err(|error| F::refused(&error.to_string()))?
                                    .starts_with(
                                        &owned
                                            .ownership
                                            .binding()
                                            .path(target)
                                            .map_err(|error| F::refused(&error.to_string()))?,
                                    )
                                    || owned
                                        .ownership
                                        .binding()
                                        .path(target)
                                        .map_err(|error| F::refused(&error.to_string()))?
                                        .starts_with(
                                            &existing
                                                .ownership
                                                .binding()
                                                .path(target)
                                                .map_err(|error| F::refused(&error.to_string()))?,
                                        )))
                        },
                    )? {
                        return Err(F::refused(
                            "persistent claims contain overlapping owned roots",
                        ));
                    }
                    selection.owners.insert(owned.directory.id.clone(), owned);
                }
            }
        }
        Ok(selected)
    }
    pub fn reconcile(
        &self,
        operation: &PersistentDirectoryOperation,
        transaction: &crate::TransactionRecord,
    ) -> Result<crate::ReconcileResult, F::Error> {
        if matches!(operation.intent.binding, PersistentBinding::SystemdState(_)) {
            return self.reconcile_native(operation, transaction);
        }
        let origin = operation
            .intent
            .previous
            .as_ref()
            .map_or(transaction.transaction_id, |owned| owned.origin);
        let Some(preparation) = self.0.load(origin, operation)? else {
            let parent = self.0.parent(operation)?;
            let name = filesystem_binding::<F>(operation)?
                .path
                .file_name()
                .ok_or_else(|| F::refused("data root needs a name"))?;
            let staging = F::creation_name(operation, origin);
            return Ok(
                if operation.intent.previous.is_none()
                    && self.0.open(&parent, name)?.is_none()
                    && self.0.open(&parent, &staging)?.is_none()
                {
                    crate::ReconcileResult::NotApplied
                } else {
                    crate::ReconcileResult::Ambiguous
                },
            );
        };
        self.bound(&preparation)?;
        let Some(object) = preparation.creation.object() else {
            let parent = self.0.parent(operation)?;
            return Ok(match self.0.open(&parent, &preparation.staging)? {
                None => crate::ReconcileResult::Incomplete,
                Some(directory)
                    if self
                        .0
                        .authenticate_creation(&directory, preparation.creation.token())? =>
                {
                    crate::ReconcileResult::Incomplete
                }
                Some(_) => crate::ReconcileResult::Ambiguous,
            });
        };
        let owned = OwnedPersistentDirectory {
            installation: operation.intent.installation,
            directory: operation.intent.directory.clone(),
            ownership: PersistentOwnership::Filesystem {
                binding: filesystem_binding::<F>(operation)?.clone(),
                object,
            },
            origin,
        };
        let receipt = PersistentDirectoryReceipt {
            stage: operation.stage,
            owned,
        };
        if self.verify(operation, &receipt).is_ok() {
            return Ok(crate::ReconcileResult::AppliedWithReceipt(
                crate::OperationReceipt::PersistentDirectory(Box::new(receipt)),
            ));
        }
        if operation.stage == PersistentDirectoryStage::Access
            && self.verify_root(operation, &receipt.owned).is_ok()
            && preparation.access != PersistentAccessState::Verified
        {
            return Ok(crate::ReconcileResult::Incomplete);
        }
        let parent = self.0.parent(operation)?;
        let name = filesystem_binding::<F>(operation)?
            .path
            .file_name()
            .ok_or_else(|| F::refused("data root needs a name"))?;
        Ok(
            if operation.intent.previous.is_none()
                && self.0.open(&parent, name)?.is_none()
                && self
                    .0
                    .open(&parent, &preparation.staging)?
                    .is_some_and(|directory| F::object(&directory).ok() == Some(object))
            {
                crate::ReconcileResult::Incomplete
            } else {
                crate::ReconcileResult::Ambiguous
            },
        )
    }
    fn bound(&self, record: &PersistentPreparation) -> Result<(), F::Error> {
        let origin = self.0.origin(record.transaction)?;
        let expected = F::creation_name(&record.operation, record.transaction);
        if origin.plan.installation != record.installation || record.installation != record.operation.intent.installation || record.staging != expected
            || !origin.plan.nodes.iter().any(|node| matches!(&node.kind,
                zup_plan::NodeKind::PersistentDirectory { operation } if operation.as_ref() == &record.operation))
        {
            return Err(F::refused("persistent directory preparation is not bound to its protected origin"));
        }
        Ok(())
    }

    fn verify_root(
        &self,
        operation: &PersistentDirectoryOperation,
        owned: &OwnedPersistentDirectory,
    ) -> Result<(), F::Error> {
        if let PersistentBinding::SystemdState(binding) = &operation.intent.binding {
            if owned.installation != operation.intent.installation
                || owned.directory != operation.intent.directory
                || owned.ownership
                    != (PersistentOwnership::SystemdState {
                        binding: binding.clone(),
                    })
            {
                return Err(F::refused(
                    "native state receipt differs from its declaration",
                ));
            }
            let origin = self
                .0
                .load(owned.origin, operation)?
                .ok_or_else(|| F::refused("native state has no protected namespace claim"))?;
            self.bound(&origin)?;
            if !same_root(&origin.operation, operation)
                || !matches!(origin.creation, PersistentCreation::NativeClaim { .. })
            {
                return Err(F::refused(
                    "native state differs from its protected creator",
                ));
            }
            return Ok(());
        }
        if owned.installation != operation.intent.installation
            || owned.directory != operation.intent.directory
            || owned_object::<F>(owned)?.operating_system()
                != filesystem_binding::<F>(operation)?
                    .path
                    .target()
                    .operating_system()
        {
            return Err(F::refused(
                "persistent directory receipt differs from its declaration",
            ));
        }
        let origin = self
            .0
            .load(owned.origin, operation)?
            .ok_or_else(|| F::refused("persistent directory has no protected creation evidence"))?;
        self.bound(&origin)?;
        if origin.installation != owned.installation
            || origin.creation.object() != Some(owned_object::<F>(owned)?)
            || origin.operation.intent.directory.id != owned.directory.id
            || !same_root(&origin.operation, operation)
        {
            return Err(F::refused(
                "persistent directory native identity differs from its creator",
            ));
        }
        let parent = self.0.parent(operation)?;
        let name = filesystem_binding::<F>(operation)?
            .path
            .file_name()
            .ok_or_else(|| F::refused("persistent directory needs a final path component"))?;
        let directory = self.0.open(&parent, name)?.ok_or_else(|| F::refused("owned persistent directory is missing; mutable data cannot be reconstructed from application payloads"))?;
        if F::object(&directory)? != owned_object::<F>(owned)? {
            return Err(F::refused(
                "persistent directory was replaced outside its installation",
            ));
        }
        Ok(())
    }

    fn prepare_root(
        &self,
        operation: &PersistentDirectoryOperation,
        transaction: &crate::TransactionRecord,
    ) -> Result<OwnedPersistentDirectory, F::Error> {
        if !transaction.plan.nodes.iter().any(|node| matches!(&node.kind,
            zup_plan::NodeKind::PersistentDirectory { operation: expected } if expected.as_ref() == operation)) {
            return Err(F::refused("persistent directory operation is not authenticated by this transaction"));
        }
        if let Some(previous) = &operation.intent.previous {
            let mut owned = previous.clone();
            owned.directory = operation.intent.directory.clone();
            self.verify_root(operation, &owned)?;
            return Ok(owned);
        }
        let parent = self.0.parent(operation)?;
        let name = filesystem_binding::<F>(operation)?
            .path
            .file_name()
            .ok_or_else(|| F::refused("persistent directory needs a final path component"))?;
        let mut preparation = match self.0.load(transaction.transaction_id, operation)? {
            Some(preparation) => preparation,
            None => {
                if self.0.open(&parent, name)?.is_some() {
                    return Err(F::refused(
                        "persistent directory already exists without authenticated ownership; existing application data is never adopted by path",
                    ));
                }
                let preparation = PersistentPreparation {
                    transaction: transaction.transaction_id,
                    installation: transaction.plan.installation,
                    operation: operation.clone(),
                    staging: F::creation_name(operation, transaction.transaction_id),
                    access: PersistentAccessState::NotRequested,
                    creation: PersistentCreation::Requested {
                        token: PersistentCreationToken(uuid::Uuid::new_v4()),
                    },
                };
                self.0.checkpoint(&preparation, None)?;
                crate::fault_injection::boundary(
                    crate::fault_injection::Boundary::PersistentCreationRequested,
                    operation.id().as_str(),
                );
                preparation
            }
        };
        self.bound(&preparation)?;
        if !same_root(&preparation.operation, operation)
            || preparation.transaction != transaction.transaction_id
        {
            return Err(F::refused(
                "persistent preparation belongs to a different operation",
            ));
        }
        if let PersistentCreation::Requested { token } = &preparation.creation {
            let directory = match self.0.open(&parent, &preparation.staging)? {
                Some(directory) if self.0.authenticate_creation(&directory, token)? => directory,
                Some(_) => {
                    return Err(F::refused(
                        "persistent creation has no authenticated native creation proof; recovery is required",
                    ));
                }
                None => self.0.create(&parent, &preparation.staging, token)?,
            };
            crate::fault_injection::boundary(
                crate::fault_injection::Boundary::PersistentDirectoryCreated,
                operation.id().as_str(),
            );
            let mut created = preparation.clone();
            created.creation = PersistentCreation::Created {
                token: token.clone(),
                object: F::object(&directory)?,
            };
            self.0.checkpoint(&created, Some(&preparation))?;
            crate::fault_injection::boundary(
                crate::fault_injection::Boundary::PersistentCreationRecorded,
                operation.id().as_str(),
            );
            preparation = created;
        }
        let object = preparation
            .creation
            .object()
            .ok_or_else(|| F::refused("persistent root creation is not recorded"))?;
        if preparation.staging == name {
            let directory = self
                .0
                .open(&parent, name)?
                .ok_or_else(|| F::refused("created persistent root is missing"))?;
            if F::object(&directory)? != object {
                return Err(F::refused("created persistent root was replaced"));
            }
        } else {
            let final_directory = self.0.open(&parent, name)?;
            let staging = self.0.open(&parent, &preparation.staging)?;
            match (final_directory, staging) {
                (None, Some(directory)) if F::object(&directory)? == object => {
                    drop(directory);
                    self.0
                        .publish(&parent, &preparation.staging, name, object)?;
                }
                (Some(directory), None) if F::object(&directory)? == object => {}
                _ => {
                    return Err(F::refused(
                        "persistent directory publication has ambiguous namespace or native identity",
                    ));
                }
            }
        }
        drop(parent);
        let owned = OwnedPersistentDirectory {
            installation: operation.intent.installation,
            directory: operation.intent.directory.clone(),
            ownership: PersistentOwnership::Filesystem {
                binding: filesystem_binding::<F>(operation)?.clone(),
                object,
            },
            origin: transaction.transaction_id,
        };
        self.verify_root(operation, &owned)?;
        Ok(owned)
    }
    pub fn apply(
        &self,
        operation: &PersistentDirectoryOperation,
        transaction: &crate::TransactionRecord,
    ) -> Result<PersistentDirectoryReceipt, F::Error> {
        if let PersistentBinding::SystemdState(binding) = &operation.intent.binding {
            if !transaction.plan.nodes.iter().any(|node| matches!(&node.kind,
                zup_plan::NodeKind::PersistentDirectory { operation: expected } if expected.as_ref() == operation)) {
                return Err(F::refused("native state operation is not authenticated by this transaction"));
            }
            let origin = operation
                .intent
                .previous
                .as_ref()
                .map_or(transaction.transaction_id, |owned| owned.origin);
            if self.0.load(origin, operation)?.is_none() {
                if operation.intent.previous.is_some() {
                    return Err(F::refused("native state creator evidence is missing"));
                }
                self.0.native_state_available(binding)?;
                self.0.checkpoint(
                    &PersistentPreparation {
                        transaction: origin,
                        installation: operation.intent.installation,
                        operation: operation.clone(),
                        staging: F::creation_name(operation, origin),
                        creation: PersistentCreation::NativeClaim {
                            token: PersistentCreationToken(uuid::Uuid::new_v4()),
                        },
                        access: PersistentAccessState::NotRequested,
                    },
                    None,
                )?;
            }
            let owned = OwnedPersistentDirectory {
                installation: operation.intent.installation,
                directory: operation.intent.directory.clone(),
                ownership: PersistentOwnership::SystemdState {
                    binding: binding.clone(),
                },
                origin,
            };
            self.verify_root(operation, &owned)?;
            if operation.stage == PersistentDirectoryStage::Access {
                let record = self
                    .0
                    .load(origin, operation)?
                    .ok_or_else(|| F::refused("native state claim is missing"))?;
                self.0.verify_native_state(binding)?;
                if record.access != PersistentAccessState::Verified {
                    let mut verified = record.clone();
                    verified.access = PersistentAccessState::Verified;
                    self.0.checkpoint(&verified, Some(&record))?;
                }
            }
            let receipt = PersistentDirectoryReceipt {
                stage: operation.stage,
                owned,
            };
            self.verify(operation, &receipt)?;
            return Ok(receipt);
        }
        let owned = self.prepare_root(operation, transaction)?;
        if operation.stage == PersistentDirectoryStage::Access {
            let mut record = self.0.load(owned.origin, operation)?.ok_or_else(|| {
                F::refused("persistent access has no protected creation evidence")
            })?;
            self.bound(&record)?;
            let parent = self.0.parent(operation)?;
            let name = filesystem_binding::<F>(operation)?
                .path
                .file_name()
                .ok_or_else(|| F::refused("persistent root has no name"))?;
            let directory = self
                .0
                .open(&parent, name)?
                .ok_or_else(|| F::refused("persistent root is missing"))?;
            if record.access != PersistentAccessState::Verified {
                if record.access == PersistentAccessState::NotRequested {
                    let mut requested = record.clone();
                    requested.access = PersistentAccessState::Requested;
                    self.0.checkpoint(&requested, Some(&record))?;
                    record = requested;
                }
                self.0
                    .provision_access(&directory, filesystem_binding::<F>(operation)?)?;
                self.0
                    .verify_access(&directory, filesystem_binding::<F>(operation)?)?;
                let mut verified = record.clone();
                verified.access = PersistentAccessState::Verified;
                self.0.checkpoint(&verified, Some(&record))?;
            } else {
                self.0
                    .verify_access(&directory, filesystem_binding::<F>(operation)?)?;
            }
        }
        let receipt = PersistentDirectoryReceipt {
            stage: operation.stage,
            owned,
        };
        self.verify(operation, &receipt)?;
        Ok(receipt)
    }

    fn reconcile_native(
        &self,
        operation: &PersistentDirectoryOperation,
        transaction: &crate::TransactionRecord,
    ) -> Result<crate::ReconcileResult, F::Error> {
        let PersistentBinding::SystemdState(binding) = &operation.intent.binding else {
            unreachable!()
        };
        let origin = operation
            .intent
            .previous
            .as_ref()
            .map_or(transaction.transaction_id, |owned| owned.origin);
        let Some(preparation) = self.0.load(origin, operation)? else {
            return Ok(if operation.intent.previous.is_none() {
                crate::ReconcileResult::NotApplied
            } else {
                crate::ReconcileResult::Ambiguous
            });
        };
        self.bound(&preparation)?;
        let receipt = PersistentDirectoryReceipt {
            stage: operation.stage,
            owned: OwnedPersistentDirectory {
                installation: operation.intent.installation,
                directory: operation.intent.directory.clone(),
                ownership: PersistentOwnership::SystemdState {
                    binding: binding.clone(),
                },
                origin,
            },
        };
        if self.verify(operation, &receipt).is_ok() {
            Ok(crate::ReconcileResult::AppliedWithReceipt(
                crate::OperationReceipt::PersistentDirectory(Box::new(receipt)),
            ))
        } else if operation.stage == PersistentDirectoryStage::Access
            && preparation.access != PersistentAccessState::Verified
            && self.verify_root(operation, &receipt.owned).is_ok()
        {
            Ok(crate::ReconcileResult::Incomplete)
        } else {
            Ok(crate::ReconcileResult::Ambiguous)
        }
    }

    pub fn verify(
        &self,
        operation: &PersistentDirectoryOperation,
        receipt: &PersistentDirectoryReceipt,
    ) -> Result<(), F::Error> {
        if receipt.stage != operation.stage {
            return Err(F::refused(
                "persistent receipt stage differs from its intent",
            ));
        }
        self.verify_root(operation, &receipt.owned)?;
        if let PersistentBinding::SystemdState(binding) = &operation.intent.binding {
            if operation.stage == PersistentDirectoryStage::Access {
                let record = self
                    .0
                    .load(receipt.owned.origin, operation)?
                    .ok_or_else(|| F::refused("native state claim is missing"))?;
                if record.access != PersistentAccessState::Verified {
                    return Err(F::refused("native state registration is not verified"));
                }
                self.0.verify_native_state(binding)?;
            }
            return Ok(());
        }
        if operation.stage == PersistentDirectoryStage::Access {
            let record = self
                .0
                .load(receipt.owned.origin, operation)?
                .ok_or_else(|| F::refused("persistent access evidence is missing"))?;
            if record.access != PersistentAccessState::Verified {
                return Err(F::refused("persistent writer access is not verified"));
            }
            let parent = self.0.parent(operation)?;
            let name = filesystem_binding::<F>(operation)?
                .path
                .file_name()
                .ok_or_else(|| F::refused("persistent root has no name"))?;
            let directory = self
                .0
                .open(&parent, name)?
                .ok_or_else(|| F::refused("persistent root is missing"))?;
            self.0
                .verify_access(&directory, filesystem_binding::<F>(operation)?)?;
        }
        Ok(())
    }
}

fn filesystem_binding<F: PersistentFilesystem>(
    operation: &PersistentDirectoryOperation,
) -> Result<&zup_plan::FilesystemDataBinding, F::Error> {
    match &operation.intent.binding {
        PersistentBinding::Filesystem(binding) => Ok(binding),
        PersistentBinding::SystemdState(_) => Err(F::refused(
            "native service state requires the native registration claim provider",
        )),
    }
}
fn owned_object<F: PersistentFilesystem>(
    owned: &OwnedPersistentDirectory,
) -> Result<NativeObjectIdentity, F::Error> {
    match owned.ownership {
        PersistentOwnership::Filesystem { object, .. } => Ok(object),
        PersistentOwnership::SystemdState { .. } => Err(F::refused(
            "native registration claim has no filesystem object",
        )),
    }
}
fn same_root(first: &PersistentDirectoryOperation, second: &PersistentDirectoryOperation) -> bool {
    first.intent.installation == second.intent.installation
        && first.intent.directory.id == second.intent.directory.id
        && first.intent.directory.location == second.intent.directory.location
        && first.intent.binding == second.intent.binding
}

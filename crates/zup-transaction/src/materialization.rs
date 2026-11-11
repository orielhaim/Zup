use zup_core::{MaterializationTree, NativeObjectIdentity, ResourceId, TransactionId};
use zup_plan::{GenerationIntent, GenerationOperation, NodeKind};

use crate::{
    GenerationAccess, GenerationReceipt, MaterializationRecord, MaterializationState,
    OperationReceipt, ReconcileResult, TransactionRecord,
};

pub trait GenerationFilesystem {
    type Directory;
    type Payload: ?Sized;
    type Error;

    fn refused(intent: &GenerationIntent, reason: &str) -> Self::Error;
    fn namespace(
        &self,
        intent: &GenerationIntent,
        create: bool,
    ) -> Result<Option<Self::Directory>, Self::Error>;
    fn open_child(
        &self,
        parent: &Self::Directory,
        name: &str,
        access: GenerationAccess,
    ) -> Result<Option<Self::Directory>, Self::Error>;
    /// Creates a private staging directory, independent of the installation scope.
    fn create_private_child(
        &self,
        parent: &Self::Directory,
        name: &str,
    ) -> Result<Self::Directory, Self::Error>;
    fn object(directory: &Self::Directory) -> Result<NativeObjectIdentity, Self::Error>;
    /// Populates staging while keeping every directory private until `publish_access`.
    fn populate_private(
        &self,
        directory: &Self::Directory,
        intent: &GenerationIntent,
        payload: &Self::Payload,
    ) -> Result<(), Self::Error>;
    fn verify_tree(
        &self,
        directory: &Self::Directory,
        intent: &GenerationIntent,
        access: GenerationAccess,
    ) -> Result<MaterializationTree, Self::Error>;
    fn publish_access(
        &self,
        directory: Self::Directory,
        intent: &GenerationIntent,
        tree: &MaterializationTree,
    ) -> Result<Self::Directory, Self::Error>;
    fn rename_noreplace(
        &self,
        parent: &Self::Directory,
        source: &str,
        destination: &str,
        expected: NativeObjectIdentity,
    ) -> Result<(), Self::Error>;
    fn sync(directory: &Self::Directory) -> Result<(), Self::Error>;
    fn read_record(
        &self,
        intent: &GenerationIntent,
    ) -> Result<Option<MaterializationRecord>, Self::Error>;
    fn write_record(
        &self,
        intent: &GenerationIntent,
        record: &MaterializationRecord,
        create_new: bool,
    ) -> Result<(), Self::Error>;
    fn origin(&self, transaction: TransactionId) -> Result<TransactionRecord, Self::Error>;
}

pub struct GenerationStorage<F> {
    filesystem: F,
}

impl<F: GenerationFilesystem> GenerationStorage<F> {
    pub fn new(filesystem: F) -> Self {
        Self { filesystem }
    }

    pub fn apply(
        &self,
        operation: GenerationOperation,
        intent: &GenerationIntent,
        transaction: TransactionId,
        payload: Option<&F::Payload>,
    ) -> Result<OperationReceipt, F::Error> {
        let authority = self.filesystem.origin(transaction)?;
        if authority.scope.authorization() != intent.privilege
            || authority.plan.installation != intent.materialization.installation()
            || !authority.plan.nodes.iter().any(|node| {
                matches!(&node.kind, NodeKind::Generation { operation: expected, intent: expected_intent }
                    if *expected == operation && expected_intent.as_ref() == intent)
            })
        {
            return Err(F::refused(intent, "generation effect has no matching protected transaction authority"));
        }
        let mut record = match self.load(intent)? {
            Some(record) => record,
            None if operation == GenerationOperation::Materialize => {
                let parent = self.namespace(intent, true)?;
                if self
                    .filesystem
                    .open_child(
                        &parent,
                        &intent.materialization.id().to_string(),
                        GenerationAccess::Public,
                    )?
                    .is_some()
                {
                    return Err(F::refused(
                        intent,
                        "unrecorded materialization destination is occupied",
                    ));
                }
                let staging = ResourceId::new(format!(
                    ".preparing-{}",
                    zup_core::MaterializationId::new_v7()
                ))
                .map_err(|error| F::refused(intent, &error.to_string()))?;
                let directory = self
                    .filesystem
                    .create_private_child(&parent, staging.as_str())?;
                boundary(
                    crate::fault_injection::Boundary::MaterializationDirectoryCreated,
                    intent,
                );
                let record = MaterializationRecord {
                    schema: 1,
                    transaction,
                    specification: intent.materialization.clone(),
                    object: F::object(&directory)?,
                    state: MaterializationState::Preparing { directory: staging },
                    retired_preparations: Vec::new(),
                };
                self.filesystem.write_record(intent, &record, true)?;
                boundary(
                    crate::fault_injection::Boundary::MaterializationPreparationRecorded,
                    intent,
                );
                record
            }
            None => {
                return Err(F::refused(
                    intent,
                    "materialization has no protected preparation evidence",
                ));
            }
        };
        let directory = self.open(&record, intent)?;
        let receipt = match operation {
            GenerationOperation::Materialize => {
                if record.transaction != transaction
                    || !matches!(record.state, MaterializationState::Preparing { .. })
                {
                    return Err(F::refused(
                        intent,
                        "committed content cannot be materialized again",
                    ));
                }
                let payload = payload.ok_or_else(|| {
                    F::refused(intent, "generation payload source is unavailable")
                })?;
                self.filesystem
                    .populate_private(&directory, intent, payload)?;
                let tree =
                    self.filesystem
                        .verify_tree(&directory, intent, GenerationAccess::Private)?;
                let staging = record
                    .state
                    .directory()
                    .expect("preparing directory")
                    .clone();
                record.state = MaterializationState::Prepared {
                    directory: staging,
                    tree,
                };
                self.write_verified(&record, intent)?;
                GenerationReceipt::Materialized {
                    observed: record.observation(),
                }
            }
            GenerationOperation::Seal => {
                if record.transaction != transaction {
                    return Err(F::refused(
                        intent,
                        "only the preparing transaction can seal content",
                    ));
                }
                let tree =
                    record.state.tree().cloned().ok_or_else(|| {
                        F::refused(intent, "unverified preparation cannot be sealed")
                    })?;
                if self
                    .filesystem
                    .verify_tree(&directory, intent, record.state.access())?
                    != tree
                {
                    return Err(F::refused(
                        intent,
                        "prepared materialization member identities changed",
                    ));
                }
                if let MaterializationState::Prepared { directory, .. } = &record.state {
                    record.state = MaterializationState::Publishing {
                        directory: directory.clone(),
                        tree: tree.clone(),
                    };
                    self.write_verified(&record, intent)?;
                    boundary(
                        crate::fault_injection::Boundary::MaterializationPublicationRequested,
                        intent,
                    );
                }
                if matches!(record.state, MaterializationState::Publishing { .. }) {
                    let directory = self.open(&record, intent)?;
                    let directory = self.filesystem.publish_access(directory, intent, &tree)?;
                    boundary(
                        crate::fault_injection::Boundary::MaterializationAccessPublished,
                        intent,
                    );
                    if self
                        .filesystem
                        .verify_tree(&directory, intent, GenerationAccess::Public)?
                        != tree
                    {
                        return Err(F::refused(
                            intent,
                            "materialization changed while publishing access",
                        ));
                    }
                    drop(directory);
                    self.publish_directory(&mut record, intent)?;
                }
                GenerationReceipt::Sealed {
                    observed: record.observation(),
                    tree,
                    seal: record
                        .fingerprint()
                        .map_err(|error| F::refused(intent, &error))?,
                }
            }
            GenerationOperation::Verify | GenerationOperation::Activate => {
                let MaterializationState::Sealed { tree: expected } = &record.state else {
                    return Err(F::refused(intent, "unsealed content cannot be selected"));
                };
                if &self
                    .filesystem
                    .verify_tree(&directory, intent, GenerationAccess::Public)?
                    != expected
                {
                    return Err(F::refused(
                        intent,
                        "sealed materialization member identities changed",
                    ));
                }
                let seal = record
                    .fingerprint()
                    .map_err(|error| F::refused(intent, &error))?;
                if operation == GenerationOperation::Verify {
                    GenerationReceipt::Verified {
                        observed: record.observation(),
                        seal,
                    }
                } else {
                    GenerationReceipt::Activated {
                        observed: record.observation(),
                        selection: intent
                            .publication
                            .selection(intent.materialization.reference())
                            .ok_or_else(|| {
                                F::refused(
                                    intent,
                                    "maintenance content has no application activation",
                                )
                            })?,
                        seal,
                    }
                }
            }
        };
        self.open(&record, intent)?;
        Ok(OperationReceipt::Generation(Box::new(receipt)))
    }

    pub fn verify(
        &self,
        intent: &GenerationIntent,
        receipt: &GenerationReceipt,
    ) -> Result<(), F::Error> {
        let record = self.observed(intent, receipt)?;
        let tree = self.filesystem.verify_tree(
            &self.open(&record, intent)?,
            intent,
            record.state.access(),
        )?;
        if let Some(expected) = record.state.tree()
            && expected != &tree
        {
            return Err(F::refused(
                intent,
                "sealed materialization member identities changed",
            ));
        }
        match receipt {
            GenerationReceipt::Materialized { .. } => {
                if matches!(record.state, MaterializationState::Preparing { .. }) {
                    return Err(F::refused(intent, "materialization has not been verified"));
                }
            }
            GenerationReceipt::Sealed { seal, .. }
            | GenerationReceipt::Verified { seal, .. }
            | GenerationReceipt::Activated { seal, .. } => {
                if !matches!(&record.state, MaterializationState::Sealed { .. })
                    || *seal
                        != record
                            .fingerprint()
                            .map_err(|error| F::refused(intent, &error))?
                {
                    return Err(F::refused(intent, "materialization seal changed"));
                }
            }
        }
        self.open(&record, intent)?;
        Ok(())
    }

    pub fn retain_after_compensation(
        &self,
        intent: &GenerationIntent,
        receipt: &GenerationReceipt,
    ) -> Result<(), F::Error> {
        self.open(&self.observed(intent, receipt)?, intent)?;
        Ok(())
    }

    pub fn reconcile(
        &self,
        operation: GenerationOperation,
        intent: &GenerationIntent,
        transaction: TransactionId,
    ) -> Result<ReconcileResult, F::Error> {
        let Some(record) = self.load(intent)? else {
            let Some(parent) = self.filesystem.namespace(intent, false)? else {
                return Ok(ReconcileResult::NotApplied);
            };
            return Ok(
                if self
                    .filesystem
                    .open_child(
                        &parent,
                        &intent.materialization.id().to_string(),
                        GenerationAccess::Public,
                    )?
                    .is_none()
                {
                    ReconcileResult::NotApplied
                } else {
                    ReconcileResult::Ambiguous
                },
            );
        };
        self.open(&record, intent)?;
        if matches!(
            operation,
            GenerationOperation::Materialize | GenerationOperation::Seal
        ) && record.transaction != transaction
        {
            return Ok(ReconcileResult::Ambiguous);
        }
        if operation == GenerationOperation::Materialize {
            let tree = match self.filesystem.verify_tree(
                &self.open(&record, intent)?,
                intent,
                record.state.access(),
            ) {
                Ok(tree) => tree,
                Err(_) if matches!(record.state, MaterializationState::Preparing { .. }) => {
                    self.restart_preparation(record, intent)?;
                    return Ok(ReconcileResult::Incomplete);
                }
                Err(error) => return Err(error),
            };
            if record
                .state
                .tree()
                .is_some_and(|expected| expected != &tree)
            {
                return Ok(ReconcileResult::Ambiguous);
            }
            let mut record = record;
            if let MaterializationState::Preparing { directory } = &record.state {
                record.state = MaterializationState::Prepared {
                    directory: directory.clone(),
                    tree,
                };
                self.write_verified(&record, intent)?;
            }
            return Ok(ReconcileResult::AppliedWithReceipt(
                OperationReceipt::Generation(Box::new(GenerationReceipt::Materialized {
                    observed: record.observation(),
                })),
            ));
        }
        if operation == GenerationOperation::Seal
            && !matches!(record.state, MaterializationState::Preparing { .. })
        {
            return Ok(ReconcileResult::AppliedWithReceipt(self.apply(
                operation,
                intent,
                transaction,
                None,
            )?));
        }
        if !matches!(&record.state, MaterializationState::Sealed { .. }) {
            return Ok(ReconcileResult::NotApplied);
        }
        Ok(ReconcileResult::AppliedWithReceipt(self.apply(
            operation,
            intent,
            transaction,
            None,
        )?))
    }

    fn restart_preparation(
        &self,
        mut record: MaterializationRecord,
        intent: &GenerationIntent,
    ) -> Result<(), F::Error> {
        let MaterializationState::Preparing {
            directory: previous,
        } = &record.state
        else {
            return Err(F::refused(
                intent,
                "published materialization content was modified",
            ));
        };
        let parent = self.namespace(intent, false)?;
        let previous_directory = self
            .filesystem
            .open_child(&parent, previous.as_str(), GenerationAccess::Private)?
            .ok_or_else(|| {
                F::refused(
                    intent,
                    "interrupted preparation is no longer at its recorded path",
                )
            })?;
        if F::object(&previous_directory)? != record.object {
            return Err(F::refused(intent, "interrupted preparation was replaced"));
        }
        let name = ResourceId::new(format!(
            ".preparing-{}",
            zup_core::MaterializationId::new_v7()
        ))
        .map_err(|error| F::refused(intent, &error.to_string()))?;
        let directory = self
            .filesystem
            .create_private_child(&parent, name.as_str())?;
        F::sync(&parent)?;
        record.retired_preparations.push(crate::RetiredPreparation {
            directory: previous.clone(),
            object: record.object,
        });
        record.object = F::object(&directory)?;
        record.state = MaterializationState::Preparing { directory: name };
        self.write_verified(&record, intent)
    }

    fn observed(
        &self,
        intent: &GenerationIntent,
        receipt: &GenerationReceipt,
    ) -> Result<MaterializationRecord, F::Error> {
        let record = self
            .load(intent)?
            .ok_or_else(|| F::refused(intent, "materialization evidence is missing"))?;
        if receipt.observed() != &record.observation() {
            return Err(F::refused(
                intent,
                "materialization object identity changed",
            ));
        }
        Ok(record)
    }

    fn namespace(&self, intent: &GenerationIntent, create: bool) -> Result<F::Directory, F::Error> {
        self.filesystem
            .namespace(intent, create)?
            .ok_or_else(|| F::refused(intent, "materialization namespace is missing"))
    }

    fn open(
        &self,
        record: &MaterializationRecord,
        intent: &GenerationIntent,
    ) -> Result<F::Directory, F::Error> {
        let parent = self.namespace(intent, false)?;
        let staging = match record.state.directory() {
            Some(name) => {
                self.filesystem
                    .open_child(&parent, name.as_str(), record.state.access())?
            }
            None => None,
        };
        let directory = match staging {
            Some(directory) => {
                if matches!(record.state, MaterializationState::Publishing { .. })
                    && self
                        .filesystem
                        .open_child(
                            &parent,
                            &intent.materialization.id().to_string(),
                            GenerationAccess::Public,
                        )?
                        .is_some()
                {
                    return Err(F::refused(
                        intent,
                        "materialization has conflicting publication names",
                    ));
                }
                directory
            }
            None if matches!(
                record.state,
                MaterializationState::Sealed { .. } | MaterializationState::Publishing { .. }
            ) =>
            {
                self.filesystem
                    .open_child(
                        &parent,
                        &intent.materialization.id().to_string(),
                        GenerationAccess::Public,
                    )?
                    .ok_or_else(|| F::refused(intent, "materialization directory is missing"))?
            }
            None => {
                return Err(F::refused(
                    intent,
                    "private preparation directory is missing",
                ));
            }
        };
        if F::object(&directory)? != record.object {
            return Err(F::refused(intent, "materialization directory was replaced"));
        }
        Ok(directory)
    }

    fn publish_directory(
        &self,
        record: &mut MaterializationRecord,
        intent: &GenerationIntent,
    ) -> Result<(), F::Error> {
        let MaterializationState::Publishing {
            directory: staging,
            tree,
        } = &record.state
        else {
            return Ok(());
        };
        let tree = tree.clone();
        let parent = self.namespace(intent, false)?;
        let target = intent.materialization.id().to_string();
        let source =
            self.filesystem
                .open_child(&parent, staging.as_str(), GenerationAccess::Public)?;
        let destination = self
            .filesystem
            .open_child(&parent, &target, GenerationAccess::Public)?;
        match (source, destination) {
            (Some(source), None) => {
                if F::object(&source)? != record.object {
                    return Err(F::refused(
                        intent,
                        "staging directory identity changed before publication",
                    ));
                }
                drop(source);
                self.filesystem.rename_noreplace(
                    &parent,
                    staging.as_str(),
                    &target,
                    record.object,
                )?;
                boundary(
                    crate::fault_injection::Boundary::MaterializationDirectoryPublished,
                    intent,
                );
            }
            (None, Some(_)) => F::sync(&parent)?,
            _ => {
                return Err(F::refused(
                    intent,
                    "materialization publication has conflicting directory observations",
                ));
            }
        }
        self.open(record, intent)?;
        record.state = MaterializationState::Sealed { tree };
        self.write_verified(record, intent)
    }

    fn write_verified(
        &self,
        record: &MaterializationRecord,
        intent: &GenerationIntent,
    ) -> Result<(), F::Error> {
        self.filesystem.write_record(intent, record, false)?;
        if self.load(intent)?.as_ref() != Some(record) {
            return Err(F::refused(
                intent,
                "materialization metadata readback differs",
            ));
        }
        Ok(())
    }

    fn load(&self, intent: &GenerationIntent) -> Result<Option<MaterializationRecord>, F::Error> {
        let Some(record) = self.filesystem.read_record(intent)? else {
            return Ok(None);
        };
        record
            .validate()
            .map_err(|error| F::refused(intent, &error))?;
        if record.specification != intent.materialization {
            return Err(F::refused(
                intent,
                "materialization evidence belongs to a different deployment",
            ));
        }
        let journal = self.filesystem.origin(record.transaction)?;
        journal
            .validate()
            .map_err(|error| F::refused(intent, &error.to_string()))?;
        if journal.plan.installation != record.specification.installation()
            || !journal.plan.nodes.iter().any(|node| matches!(&node.kind, NodeKind::Generation { operation: GenerationOperation::Materialize, intent: original } if original.materialization == record.specification)) {
            return Err(F::refused(intent, "materialization has no authenticated preparation intent"));
        }
        for node in &journal.plan.nodes {
            if !matches!(&node.kind, NodeKind::Generation { intent: original, .. }
                if original.materialization == record.specification)
            {
                continue;
            }
            if let Some(OperationReceipt::Generation(receipt)) = journal.receipt(&node.id) {
                if receipt.observed() != &record.observation() {
                    return Err(F::refused(
                        intent,
                        "materialization differs from its protected native receipt",
                    ));
                }
                if matches!(receipt.as_ref(), GenerationReceipt::Materialized { .. })
                    && matches!(record.state, MaterializationState::Preparing { .. })
                {
                    return Err(F::refused(
                        intent,
                        "materialization preparation state regressed after publication",
                    ));
                }
                if let GenerationReceipt::Sealed { seal, tree, .. } = receipt.as_ref()
                    && (!matches!(&record.state, MaterializationState::Sealed { tree: expected } if expected == tree)
                        || *seal
                            != record
                                .fingerprint()
                                .map_err(|error| F::refused(intent, &error))?)
                {
                    return Err(F::refused(
                        intent,
                        "materialization seal differs from its originating journal",
                    ));
                }
            }
        }
        Ok(Some(record))
    }
}

fn boundary(point: crate::fault_injection::Boundary, intent: &GenerationIntent) {
    let operation = match point {
        crate::fault_injection::Boundary::MaterializationPublicationRequested
        | crate::fault_injection::Boundary::MaterializationMemberExposed
        | crate::fault_injection::Boundary::MaterializationAccessPublished
        | crate::fault_injection::Boundary::MaterializationDirectoryPublished => {
            GenerationOperation::Seal
        }
        _ => GenerationOperation::Materialize,
    };
    crate::fault_injection::boundary(point, intent.operation_id(operation).as_str());
}

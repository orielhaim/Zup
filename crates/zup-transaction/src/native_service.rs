use serde::{Deserialize, Serialize};
use zup_core::{ObservedServiceRegistration, ResourceKey, Sha256Digest};
use zup_plan::{NativeServiceOperation, ServiceState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeServiceReceipt {
    pub intent: Sha256Digest,
    pub configuration: ServiceState,
    pub registration: ObservedServiceRegistration,
}

impl NativeServiceReceipt {
    pub fn matches(&self, operation: &NativeServiceOperation) -> bool {
        let ResourceKey::Service { id } = operation.key() else {
            return false;
        };
        self.intent == operation.fingerprint()
            && self.configuration == operation.configurations().1
            && if self.configuration == ServiceState::Absent {
                self.registration == ObservedServiceRegistration::Unclaimed
            } else {
                self.registration.matches(operation.registration(), id)
            }
    }
}

//! HarnessRegistry — the engine's Copilot and test-harness catalog.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use zeron_harness::{Harness, HarnessError, mock::MockHarness};
use zeron_proto::{HarnessId, ReasoningLevel, SteeringMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default = "default_installed")]
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn default_installed() -> bool {
    true
}

pub fn descriptor_enabled(descriptor: &HarnessDescriptor) -> bool {
    descriptor.enabled.unwrap_or(descriptor.installed)
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        supports_steering: harness.supports_steering(),
        steering_mode: harness.steering_mode(),
        reasoning_levels: harness.reasoning_levels().to_vec(),
        installed: harness.installed(),
        enabled: Some(harness.installed()),
    }
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;
type InstalledProbe = Box<dyn Fn() -> bool + Send + Sync>;

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        descriptor: HarnessDescriptor,
        installed: InstalledProbe,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self
            .slots()
            .insert(id.clone(), Slot::Ready(harness))
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn register_copilot(&self, credentials: Arc<dyn zeron_harness::CopilotCredentialSource>) {
        let id = HarnessId::Copilot;
        self.slots().insert(
            id.clone(),
            Slot::Ready(Arc::new(zeron_harness::CopilotHarness::new(credentials))),
        );
        let mut order = self.order();
        order.retain(|registered| *registered != id);
        order.insert(0, id);
    }

    pub fn register_lazy(
        &self,
        descriptor: HarnessDescriptor,
        installed: InstalledProbe,
        factory: Factory,
    ) {
        let id = descriptor.id.clone();
        if self
            .slots()
            .insert(
                id.clone(),
                Slot::Lazy {
                    descriptor,
                    installed,
                    factory,
                },
            )
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn resolve(&self, id: HarnessId) -> Result<Arc<dyn Harness>, HarnessError> {
        let mut slots = self.slots();
        match slots.get(&id) {
            Some(Slot::Ready(harness)) => Ok(harness.clone()),
            Some(Slot::Lazy { factory, .. }) => {
                let harness = factory()?;
                slots.insert(id, Slot::Ready(harness.clone()));
                Ok(harness)
            }
            None => Err(HarnessError::NotInstalled(format!("{id:?}"))),
        }
    }

    pub fn descriptors(&self) -> Vec<HarnessDescriptor> {
        let slots = self.slots();
        self.order()
            .iter()
            .filter_map(|id| match slots.get(id) {
                Some(Slot::Ready(harness)) => Some(describe(harness.as_ref())),
                Some(Slot::Lazy {
                    descriptor,
                    installed,
                    ..
                }) => Some(HarnessDescriptor {
                    installed: installed(),
                    enabled: Some(installed()),
                    ..descriptor.clone()
                }),
                None => None,
            })
            .collect()
    }
}

pub fn default_registry() -> HarnessRegistry {
    let registry = HarnessRegistry::new();
    registry.register_copilot(Arc::new(crate::CopilotCredentialHolder::default()));
    registry.register(Arc::new(MockHarness { script: Vec::new() }));
    registry
}

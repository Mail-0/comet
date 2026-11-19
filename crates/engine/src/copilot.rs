use std::sync::{Arc, Mutex, PoisonError, Weak};

use zeron_copilot::CopilotCredentials;

#[derive(Clone, Default)]
pub struct CopilotCredentialHolder {
    credentials: Arc<Mutex<Option<CopilotCredentials>>>,
    bound_source: Arc<Mutex<Option<Weak<dyn zeron_harness::BoundConversationSource>>>>,
}

impl std::fmt::Debug for CopilotCredentialHolder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CopilotCredentialHolder")
            .field("credentials", &self.snapshot())
            .finish()
    }
}

impl CopilotCredentialHolder {
    pub(crate) fn set_bound_source(&self, source: Weak<dyn zeron_harness::BoundConversationSource>) {
        *self.bound_source.lock().unwrap_or_else(PoisonError::into_inner) = Some(source);
    }
    pub(crate) fn same_holder(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.credentials, &other.credentials)
    }

    pub fn set(&self, credentials: CopilotCredentials) {
        *self
            .credentials
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(credentials);
    }

    pub fn clear(&self) {
        *self
            .credentials
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub fn snapshot(&self) -> Option<CopilotCredentials> {
        self.credentials
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl zeron_harness::CopilotCredentialSource for CopilotCredentialHolder {
    fn bound_source(&self) -> Option<Arc<dyn zeron_harness::BoundConversationSource>> {
        self.bound_source.lock().unwrap_or_else(PoisonError::into_inner).as_ref()?.upgrade()
    }
    fn snapshot(&self) -> Option<CopilotCredentials> {
        self.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_can_be_set_snapshotted_and_cleared() {
        let holder = CopilotCredentialHolder::default();
        assert!(holder.snapshot().is_none());
        holder.set(CopilotCredentials {
            base_url: "https://copilot.example".into(),
            access_token: "secret".into(),
        });
        assert_eq!(
            holder.snapshot().map(|credentials| credentials.base_url),
            Some("https://copilot.example".into())
        );
        assert_eq!(
            holder
                .snapshot()
                .map(|credentials| credentials.access_token),
            Some("secret".into())
        );
        holder.clear();
        assert!(holder.snapshot().is_none());
    }

    #[test]
    fn debug_redacts_the_access_token() {
        let holder = CopilotCredentialHolder::default();
        holder.set(CopilotCredentials {
            base_url: "https://copilot.example".into(),
            access_token: "secret".into(),
        });
        let debug = format!("{holder:?}");
        assert!(debug.contains("https://copilot.example"));
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("secret"));
    }
}

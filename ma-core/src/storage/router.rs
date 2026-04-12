// /Memory-Archive/ma-core/src/storage/router.rs

use std::collections::HashMap;
use std::sync::Arc;

use crate::registry::schema::SessionRecord;

pub struct StorageRouter {
    pool: HashMap<String, Arc<dyn super::StorageBackend>>,
    rules: Vec<RoutingRule>,
}

pub(super) struct RoutingRule {
    pub(super) matcher: RuleMatcher,
    pub(super) backend_name: String,
}

pub(super) enum RuleMatcher {
    TenantPrefix(String),
    Default,
}

impl StorageRouter {
    pub(super) fn new(
        pool: HashMap<String, Arc<dyn super::StorageBackend>>,
        rules: Vec<RoutingRule>,
    ) -> Self {
        Self { pool, rules }
    }

    /// Select a backend for a new session registration.
    /// Returns (backend_name, backend_arc).
    /// Rules are evaluated in order; the first match wins.
    /// Falls back to the first pool entry if no rule matches.
    pub fn resolve(&self, tenant_id: &str) -> (String, Arc<dyn super::StorageBackend>) {
        for rule in &self.rules {
            let matched = match &rule.matcher {
                RuleMatcher::TenantPrefix(prefix) => {
                    !tenant_id.is_empty() && tenant_id.starts_with(prefix.as_str())
                }
                RuleMatcher::Default => true,
            };
            if matched {
                if let Some(backend) = self.pool.get(&rule.backend_name) {
                    return (rule.backend_name.clone(), backend.clone());
                }
                tracing::warn!(
                    backend = %rule.backend_name,
                    "StorageRouter: rule references unknown backend — skipping rule"
                );
            }
        }
        if let Some((name, backend)) = self.pool.iter().next() {
            tracing::warn!(
                backend = %name,
                "StorageRouter: no routing rule matched — falling back to first backend in pool"
            );
            return (name.clone(), backend.clone());
        }
        panic!("StorageRouter pool is empty — this is a startup bug");
    }

    /// Resolve the backend for an existing session using its pinned storage_backend field.
    /// Falls back to resolve(tenant_id) when the stored name is empty or absent from pool
    /// (backwards compatibility: sessions stored before this field was added).
    pub fn resolve_for_session(&self, record: &SessionRecord) -> Arc<dyn super::StorageBackend> {
        if !record.storage_backend.is_empty() {
            if let Some(backend) = self.pool.get(&record.storage_backend) {
                return backend.clone();
            }
            tracing::warn!(
                session_id = %record.session_id,
                backend = %record.storage_backend,
                "StorageRouter: pinned backend not found in pool — re-resolving from tenant_id"
            );
        }
        self.resolve(&record.tenant_id).1
    }

    /// Look up a named backend directly.
    #[allow(dead_code)]
    pub fn get_by_name(&self, name: &str) -> Option<Arc<dyn super::StorageBackend>> {
        self.pool.get(name).cloned()
    }
}
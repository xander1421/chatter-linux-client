//! Organization state.
//!
//! Reactive organization state for UI binding — tracks current org context,
//! user's org list, and membership info.

use tokio::sync::broadcast;

use crate::state::store::Store;

/// Organization state data.
///
/// Tracks the user's organization memberships and the currently active org.
/// The `current_org_id` is injected as the `X-Org-Id` header by the interceptor.
#[derive(Clone, Debug, Default)]
pub struct OrgStateData {
    /// Currently active organization ID, if any.
    pub current_org_id: Option<String>,
    /// Name of the currently active organization.
    pub current_org_name: Option<String>,
    /// List of organizations the user belongs to (id, name, slug).
    pub organizations: Vec<OrgSummary>,
    /// Whether org data is currently being loaded.
    pub is_loading: bool,
}

/// Lightweight org summary for the org list/switcher.
#[derive(Clone, Debug, PartialEq)]
pub struct OrgSummary {
    /// Organization ID (UUID).
    pub id: String,
    /// Organization display name.
    pub name: String,
    /// Organization slug (URL-safe identifier).
    pub slug: String,
    /// User's role in this organization.
    pub viewer_role: i32,
    /// Number of members.
    pub member_count: i32,
}

/// Reactive organization state manager.
///
/// Provides a reactive wrapper around `OrgStateData` that allows
/// UI components to subscribe to organization state changes.
///
/// # Example
///
/// ```ignore
/// let org_state = OrgState::new();
///
/// // UI subscribes to org changes
/// let mut rx = org_state.subscribe();
///
/// // Service updates state after fetching orgs
/// org_state.set_organizations(orgs);
/// org_state.set_current_org("org-uuid", "My Org");
/// ```
#[derive(Clone)]
pub struct OrgState {
    store: Store<OrgStateData>,
}

impl OrgState {
    /// Create a new org state with no active organization.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(OrgStateData::default()),
        }
    }

    /// Get the current organization state.
    #[must_use]
    pub fn get(&self) -> OrgStateData {
        self.store.get()
    }

    /// Subscribe to organization state changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<OrgStateData>> {
        self.store.subscribe()
    }

    /// Get the current org ID, if set.
    #[must_use]
    pub fn current_org_id(&self) -> Option<String> {
        self.store.get().current_org_id
    }

    /// Set the active organization.
    pub fn set_current_org(&self, org_id: &str, org_name: &str) {
        self.store.update(|state| {
            state.current_org_id = Some(org_id.to_string());
            state.current_org_name = Some(org_name.to_string());
        });
    }

    /// Clear the active organization (e.g. on logout).
    pub fn clear_current_org(&self) {
        self.store.update(|state| {
            state.current_org_id = None;
            state.current_org_name = None;
        });
    }

    /// Set the full organization list (after fetching from server).
    pub fn set_organizations(&self, orgs: Vec<OrgSummary>) {
        self.store.update(|state| {
            state.organizations = orgs;
            state.is_loading = false;
        });
    }

    /// Set loading state.
    pub fn set_loading(&self, loading: bool) {
        self.store.update(|state| {
            state.is_loading = loading;
        });
    }

    /// Clear all org state (e.g. on logout).
    pub fn clear(&self) {
        self.store.update(|state| {
            state.current_org_id = None;
            state.current_org_name = None;
            state.organizations.clear();
            state.is_loading = false;
        });
    }
}

impl Default for OrgState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_org_state_default_is_empty() {
        let state = OrgState::new();
        let data = state.get();

        assert!(data.current_org_id.is_none());
        assert!(data.current_org_name.is_none());
        assert!(data.organizations.is_empty());
        assert!(!data.is_loading);
    }

    #[test]
    fn test_org_state_set_current_org() {
        let state = OrgState::new();

        state.set_current_org("org-123", "Test Org");

        let data = state.get();
        assert_eq!(data.current_org_id, Some("org-123".to_string()));
        assert_eq!(data.current_org_name, Some("Test Org".to_string()));
    }

    #[test]
    fn test_org_state_set_organizations() {
        let state = OrgState::new();
        let orgs = vec![
            OrgSummary {
                id: "org-1".to_string(),
                name: "First Org".to_string(),
                slug: "first-org".to_string(),
                viewer_role: 0,
                member_count: 5,
            },
            OrgSummary {
                id: "org-2".to_string(),
                name: "Second Org".to_string(),
                slug: "second-org".to_string(),
                viewer_role: 1,
                member_count: 10,
            },
        ];

        state.set_organizations(orgs);

        let data = state.get();
        assert_eq!(data.organizations.len(), 2);
        assert_eq!(data.organizations[0].name, "First Org");
        assert_eq!(data.organizations[1].slug, "second-org");
    }

    #[test]
    fn test_org_state_clear() {
        let state = OrgState::new();

        state.set_current_org("org-123", "Test Org");
        state.set_organizations(vec![OrgSummary {
            id: "org-123".to_string(),
            name: "Test Org".to_string(),
            slug: "test-org".to_string(),
            viewer_role: 0,
            member_count: 1,
        }]);

        state.clear();

        let data = state.get();
        assert!(data.current_org_id.is_none());
        assert!(data.organizations.is_empty());
    }

    #[test]
    fn test_org_state_clone_shares_state() {
        let state1 = OrgState::new();
        let state2 = state1.clone();

        state1.set_current_org("org-123", "Test Org");

        assert_eq!(state2.current_org_id(), Some("org-123".to_string()));
    }
}

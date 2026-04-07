//! Organization service.
//!
//! Business logic for organization CRUD, membership, and active org context.

use std::sync::Arc;

use crate::error::NetworkError;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::current_user::CurrentUserProvider;
use crate::proto::organization_v1::Organization;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Error type for organization operations.
#[derive(Debug, thiserror::Error)]
pub enum OrganizationError {
    /// Not logged in.
    #[error("not logged in")]
    NotLoggedIn,
    /// Network error.
    #[error("network: {0}")]
    Network(#[from] NetworkError),
    /// Storage error.
    #[error("storage: {0}")]
    Storage(String),
}

/// Organization service for managing organizations and membership.
pub struct OrganizationService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
}

impl OrganizationService {
    /// Create a new OrganizationService.
    pub fn new(grpc_pool: Arc<GrpcClientPool>, settings_repo: Arc<SettingsRepository>) -> Self {
        Self { grpc_pool, settings_repo }
    }

    fn org_client(&self) -> crate::network::grpc::OrganizationClient {
        self.grpc_pool.organization_client()
    }

    /// List organizations the current user belongs to.
    pub async fn list_user_organizations(&self) -> Result<Vec<Organization>, OrganizationError> {
        let _ = self.settings_repo.current_user_id()
            .map_err(|_| OrganizationError::NotLoggedIn)?;

        let orgs = self.org_client().list_user_organizations().await?;
        Ok(orgs)
    }

    /// Get a single organization by ID.
    pub async fn get_organization(&self, org_id: &str) -> Result<Organization, OrganizationError> {
        let org = self.org_client().get_organization(org_id).await?;
        Ok(org)
    }

    /// List members of an organization.
    pub async fn list_members(
        &self,
        org_id: &str,
    ) -> Result<crate::proto::organization_v1::ListMembersResponse, OrganizationError> {
        let resp = self.org_client().list_members(org_id).await?;
        Ok(resp)
    }

    /// Set the active organization context.
    ///
    /// Stores the active org ID in settings for use by other services.
    pub fn set_active_organization(&self, org_id: Option<&str>) -> Result<(), OrganizationError> {
        self.settings_repo
            .set("active_organization_id", &org_id.unwrap_or("").to_string())
            .map_err(|e| OrganizationError::Storage(e.to_string()))
    }

    /// Get the active organization ID.
    pub fn active_organization_id(&self) -> Option<String> {
        self.settings_repo
            .get::<String>("active_organization_id")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
    }
}

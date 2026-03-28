//! Organization gRPC client.
//!
//! Client for organization operations including CRUD, membership management,
//! and listing user organizations.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::organization_v1::{
    organization_service_client::OrganizationServiceClient,
    CreateOrganizationRequest, CreateOrganizationResponse,
    GetOrganizationRequest, GetOrganizationResponse,
    UpdateOrganizationRequest, UpdateOrganizationResponse,
    DeleteOrganizationRequest,
    ListUserOrganizationsRequest, ListUserOrganizationsResponse,
    InviteMemberRequest, InviteMemberResponse,
    AcceptInvitationRequest, AcceptInvitationResponse,
    RemoveMemberRequest,
    UpdateMemberRoleRequest,
    ListMembersRequest, ListMembersResponse,
    GetOrgSettingsRequest, GetOrgSettingsResponse,
    GetOrgDashboardRequest, OrgDashboard,
    Organization,
};

/// Organization service client wrapper.
///
/// Wraps the generated `OrganizationServiceClient` with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut client = OrganizationClient::new(pool.channel(), pool.auth_clone());
/// let orgs = client.list_user_organizations().await?;
/// ```
pub struct OrganizationClient {
    inner: OrganizationServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl OrganizationClient {
    /// Create a new OrganizationClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: OrganizationServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Org CRUD RPCs ====================

    /// Create a new organization.
    pub async fn create_organization(
        &mut self,
        name: &str,
        slug: &str,
        description: &str,
    ) -> Result<Organization, NetworkError> {
        let request = CreateOrganizationRequest {
            name: name.to_string(),
            slug: slug.to_string(),
            description: description.to_string(),
        };

        let response: CreateOrganizationResponse = self
            .inner
            .create_organization(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        response
            .organization
            .ok_or_else(|| NetworkError::InvalidRequest("no organization in response".into()))
    }

    /// Get an organization by ID.
    pub async fn get_organization(&mut self, org_id: &str) -> Result<Organization, NetworkError> {
        let request = GetOrganizationRequest {
            org_id: org_id.to_string(),
        };

        let response: GetOrganizationResponse = self
            .inner
            .get_organization(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        response
            .organization
            .ok_or_else(|| NetworkError::InvalidRequest("no organization in response".into()))
    }

    /// Update an organization.
    pub async fn update_organization(
        &mut self,
        org_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        avatar_url: Option<&str>,
    ) -> Result<Organization, NetworkError> {
        let request = UpdateOrganizationRequest {
            org_id: org_id.to_string(),
            name: name.map(|s| s.to_string()),
            description: description.map(|s| s.to_string()),
            avatar_url: avatar_url.map(|s| s.to_string()),
        };

        let response: UpdateOrganizationResponse = self
            .inner
            .update_organization(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        response
            .organization
            .ok_or_else(|| NetworkError::InvalidRequest("no organization in response".into()))
    }

    /// Delete an organization.
    pub async fn delete_organization(&mut self, org_id: &str) -> Result<(), NetworkError> {
        let request = DeleteOrganizationRequest {
            org_id: org_id.to_string(),
        };

        self.inner
            .delete_organization(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List all organizations the current user belongs to.
    pub async fn list_user_organizations(&mut self) -> Result<Vec<Organization>, NetworkError> {
        let request = ListUserOrganizationsRequest {};

        let response: ListUserOrganizationsResponse = self
            .inner
            .list_user_organizations(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.organizations)
    }

    // ==================== Membership RPCs ====================

    /// Invite a member to an organization.
    pub async fn invite_member(
        &mut self,
        org_id: &str,
        email: &str,
        role: i32,
    ) -> Result<InviteMemberResponse, NetworkError> {
        let request = InviteMemberRequest {
            org_id: org_id.to_string(),
            email: email.to_string(),
            role,
        };

        self.inner
            .invite_member(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Accept an organization invitation.
    pub async fn accept_invitation(
        &mut self,
        invite_token: &str,
    ) -> Result<AcceptInvitationResponse, NetworkError> {
        let request = AcceptInvitationRequest {
            invite_token: invite_token.to_string(),
        };

        self.inner
            .accept_invitation(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Remove a member from an organization.
    pub async fn remove_member(
        &mut self,
        org_id: &str,
        user_id: &str,
    ) -> Result<(), NetworkError> {
        let request = RemoveMemberRequest {
            org_id: org_id.to_string(),
            user_id: user_id.to_string(),
        };

        self.inner
            .remove_member(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Update a member's role in an organization.
    pub async fn update_member_role(
        &mut self,
        org_id: &str,
        user_id: &str,
        role: i32,
    ) -> Result<(), NetworkError> {
        let request = UpdateMemberRoleRequest {
            org_id: org_id.to_string(),
            user_id: user_id.to_string(),
            role,
        };

        self.inner
            .update_member_role(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List members of an organization.
    pub async fn list_members(
        &mut self,
        org_id: &str,
    ) -> Result<ListMembersResponse, NetworkError> {
        let request = ListMembersRequest {
            org_id: org_id.to_string(),
            page_size: 0,
            page_token: String::new(),
            search_query: None,
            role_filter: None,
        };

        self.inner
            .list_members(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get organization settings.
    pub async fn get_org_settings(
        &mut self,
        org_id: &str,
    ) -> Result<GetOrgSettingsResponse, NetworkError> {
        let request = GetOrgSettingsRequest {
            org_id: org_id.to_string(),
        };

        self.inner
            .get_org_settings(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get organization dashboard analytics.
    pub async fn get_org_dashboard(
        &mut self,
        org_id: &str,
    ) -> Result<OrgDashboard, NetworkError> {
        let request = GetOrgDashboardRequest {
            org_id: org_id.to_string(),
        };

        self.inner
            .get_org_dashboard(request)
            .await
            .map(|r| {
                let resp = r.into_inner();
                resp.dashboard.unwrap_or_default()
            })
            .map_err(|s| NetworkError::from_status(&s))
    }
}

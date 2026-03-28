// @generated
/// Generated client implementations.
pub mod organization_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct OrganizationServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl OrganizationServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> OrganizationServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> OrganizationServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            OrganizationServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        /** Org CRUD
*/
        pub async fn create_organization(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateOrganizationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateOrganizationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/CreateOrganization",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "CreateOrganization",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_organization(
            &mut self,
            request: impl tonic::IntoRequest<super::GetOrganizationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetOrganizationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/GetOrganization",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "GetOrganization",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn update_organization(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateOrganizationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdateOrganizationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/UpdateOrganization",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "UpdateOrganization",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn delete_organization(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteOrganizationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DeleteOrganizationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/DeleteOrganization",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "DeleteOrganization",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_user_organizations(
            &mut self,
            request: impl tonic::IntoRequest<super::ListUserOrganizationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListUserOrganizationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/ListUserOrganizations",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "ListUserOrganizations",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Membership
*/
        pub async fn invite_member(
            &mut self,
            request: impl tonic::IntoRequest<super::InviteMemberRequest>,
        ) -> std::result::Result<
            tonic::Response<super::InviteMemberResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/InviteMember",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "InviteMember",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn accept_invitation(
            &mut self,
            request: impl tonic::IntoRequest<super::AcceptInvitationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AcceptInvitationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/AcceptInvitation",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "AcceptInvitation",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn remove_member(
            &mut self,
            request: impl tonic::IntoRequest<super::RemoveMemberRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RemoveMemberResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/RemoveMember",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "RemoveMember",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn update_member_role(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateMemberRoleRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdateMemberRoleResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/UpdateMemberRole",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "UpdateMemberRole",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_members(
            &mut self,
            request: impl tonic::IntoRequest<super::ListMembersRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListMembersResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/ListMembers",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "ListMembers",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn bulk_invite_members(
            &mut self,
            request: impl tonic::IntoRequest<super::BulkInviteMembersRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BulkInviteMembersResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/BulkInviteMembers",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "BulkInviteMembers",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Cross-org channel links
*/
        pub async fn create_channel_link(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateChannelLinkRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateChannelLinkResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/CreateChannelLink",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "CreateChannelLink",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn approve_channel_link(
            &mut self,
            request: impl tonic::IntoRequest<super::ApproveChannelLinkRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ApproveChannelLinkResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/ApproveChannelLink",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "ApproveChannelLink",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn revoke_channel_link(
            &mut self,
            request: impl tonic::IntoRequest<super::RevokeChannelLinkRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RevokeChannelLinkResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/RevokeChannelLink",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "RevokeChannelLink",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_channel_links(
            &mut self,
            request: impl tonic::IntoRequest<super::ListChannelLinksRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListChannelLinksResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/ListChannelLinks",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "ListChannelLinks",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_pending_channel_links(
            &mut self,
            request: impl tonic::IntoRequest<super::ListPendingChannelLinksRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListPendingChannelLinksResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/ListPendingChannelLinks",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "ListPendingChannelLinks",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_org_settings(
            &mut self,
            request: impl tonic::IntoRequest<super::GetOrgSettingsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetOrgSettingsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/GetOrgSettings",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "GetOrgSettings",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_org_settings(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateOrgSettingsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdateOrgSettingsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/UpdateOrgSettings",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "UpdateOrgSettings",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_audit_log(
            &mut self,
            request: impl tonic::IntoRequest<super::GetAuditLogRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetAuditLogResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/GetAuditLog",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "GetAuditLog",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_org_dashboard(
            &mut self,
            request: impl tonic::IntoRequest<super::GetOrgDashboardRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetOrgDashboardResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/GetOrgDashboard",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "GetOrgDashboard",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn transfer_ownership(
            &mut self,
            request: impl tonic::IntoRequest<super::TransferOwnershipRequest>,
        ) -> std::result::Result<
            tonic::Response<super::TransferOwnershipResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.organization.OrganizationService/TransferOwnership",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.organization.OrganizationService",
                        "TransferOwnership",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}

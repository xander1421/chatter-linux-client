use super::*;

impl ChatterClient {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    // ── Services (return &Arc<T> so .clone() yields Arc<T>) ─────────────
    //
    // Default visibility is `pub(crate)` — chatter-ui MUST route through the
    // high-level façade methods below, not raw service accessors, per the
    // `feedback_service_layer_architecture` memory rule. Integration tests
    // (the `tests/` directory, external to the lib) enable the `testing`
    // feature, which widens these to `pub` so fixtures can wire real
    // services against a live server.
    crate::service_accessor!(auth, AuthService, auth_service);
    crate::service_accessor!(messages, MessageService, message_service);
    crate::service_accessor!(channels, ChannelService, channel_service);
    crate::service_accessor!(users, UserService, user_service);
    crate::service_accessor!(attachments, AttachmentService, attachment_service);
    crate::service_accessor!(pins, PinService, pin_service);
    crate::service_accessor!(sessions, SessionService, session_service);
    crate::service_accessor!(anon_dms, AnonDmService, anon_dm_service);
    crate::service_accessor!(anon_groups, AnonGroupService, anon_group_service);
    crate::service_accessor!(signal, SignalService, signal_service);
    crate::service_accessor!(ai, AIService, ai_service);
    #[cfg(feature = "jarvis")]
    crate::service_accessor!(jarvis, JarvisService, jarvis_service);
    #[cfg(feature = "mesh")]
    crate::service_accessor!(mesh, crate::services::mesh::MeshService, mesh_service);

}

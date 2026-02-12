//! Sessions modal callbacks.

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_linux::app::AppContext;

use crate::{MainWindow, SessionInfo};

pub fn wire_session_callbacks(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();

    // View sessions
    let weak_window = window.as_weak();
    let grpc_pool_sessions = grpc_pool.clone();
    let settings_repo_sessions = settings_repo.clone();

    window.on_settings_view_sessions(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_sessions.clone();
        let settings_repo = settings_repo_sessions.clone();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_sessions_loading(true);
                    w.set_active_modal("sessions".into());
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo);

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service.list_sessions(&mut auth_client).await {
                Ok(sessions) => {
                    let slint_sessions: Vec<SessionInfo> = sessions
                        .into_iter()
                        .map(|s| SessionInfo {
                            device_id: s.device_id as i32,
                            device_info: s.device_info.into(),
                            ip_address: s.ip_address.into(),
                            last_used: s.last_used.format("%Y-%m-%d %H:%M").to_string().into(),
                            created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                            is_current: s.is_current,
                        })
                        .collect();

                    let session_count = slint_sessions.len() as i32;

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                            w.set_sessions_loading(false);
                            w.set_settings_session_count(session_count);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to list sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke single session
    let weak_window = window.as_weak();
    let grpc_pool_revoke = grpc_pool.clone();
    let settings_repo_revoke = settings_repo.clone();

    window.on_revoke_session(move |device_id: i32| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_revoke.clone();
        let settings_repo = settings_repo_revoke.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo.clone());

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service
                .revoke_session(&mut auth_client, device_id as u32)
                .await
            {
                Ok(()) => {
                    match session_service.list_sessions(&mut auth_client).await {
                        Ok(sessions) => {
                            let slint_sessions: Vec<SessionInfo> = sessions
                                .into_iter()
                                .map(|s| SessionInfo {
                                    device_id: s.device_id as i32,
                                    device_info: s.device_info.into(),
                                    ip_address: s.ip_address.into(),
                                    last_used: s
                                        .last_used
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                        .into(),
                                    created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                                    is_current: s.is_current,
                                })
                                .collect();

                            let session_count = slint_sessions.len() as i32;

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                                    w.set_sessions_revoking(false);
                                    w.set_settings_session_count(session_count);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                    tracing::info!("Revoked session for device {}", device_id);
                }
                Err(e) => {
                    tracing::error!("Failed to revoke session: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke all other sessions
    let weak_window = window.as_weak();
    let grpc_pool_revoke_all = grpc_pool.clone();
    let settings_repo_revoke_all = settings_repo.clone();

    window.on_revoke_all_sessions(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_revoke_all.clone();
        let settings_repo = settings_repo_revoke_all.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo.clone());

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service
                .revoke_all_other_sessions(&mut auth_client)
                .await
            {
                Ok(()) => {
                    match session_service.list_sessions(&mut auth_client).await {
                        Ok(sessions) => {
                            let slint_sessions: Vec<SessionInfo> = sessions
                                .into_iter()
                                .map(|s| SessionInfo {
                                    device_id: s.device_id as i32,
                                    device_info: s.device_info.into(),
                                    ip_address: s.ip_address.into(),
                                    last_used: s
                                        .last_used
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                        .into(),
                                    created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                                    is_current: s.is_current,
                                })
                                .collect();

                            let session_count = slint_sessions.len() as i32;

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                                    w.set_sessions_revoking(false);
                                    w.set_settings_session_count(session_count);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke all: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                    tracing::info!("Revoked all other sessions");
                }
                Err(e) => {
                    tracing::error!("Failed to revoke all sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });
}

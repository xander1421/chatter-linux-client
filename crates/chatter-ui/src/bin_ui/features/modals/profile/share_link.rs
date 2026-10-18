use super::*;

pub(super) fn set_share_link_props(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, url: &str) {
    w.global::<crate::ModalHostState>()
        .set_profile_share_link(url.into());
    match ui_ctx
        .username_link_qr(url, 150)
        .map_err(|e| e.to_string())
        .and_then(|png| {
            crate::bin_ui::slint_conversions::memory_to_slint_image(&png).map_err(|e| e.to_string())
        }) {
        Ok(img) => w
            .global::<crate::ModalHostState>()
            .set_profile_share_qr(img),
        Err(e) => tracing::error!("Failed to render username-link QR: {}", e),
    }
}

/// Wire the username-link share callbacks (self profile view).
pub(super) fn wire_share_link(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Generate: create a link if none exists yet (first press), otherwise
    // re-show the stored one. Rotation is the explicit Reset button.
    let weak_window = window.as_weak();
    let ui_ctx_gen = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_generate_share_link(move || {
            spawn_create_link(weak_window.clone(), ui_ctx_gen.clone(), false);
        });

    // Reset: always create a fresh link (server-side rotation — previously
    // shared URLs stop resolving).
    let weak_window = window.as_weak();
    let ui_ctx_reset = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_reset_share_link(move || {
            spawn_create_link(weak_window.clone(), ui_ctx_reset.clone(), true);
        });

    // Copy: same clipboard strategy as the friend-code copy (wl-copy → xclip).
    let weak_window = window.as_weak();
    window
        .global::<crate::ModalHostState>()
        .on_profile_copy_share_link(move || {
            if let Some(w) = weak_window.upgrade() {
                let url = w
                    .global::<crate::ModalHostState>()
                    .get_profile_share_link()
                    .to_string();
                if url.is_empty() {
                    return;
                }
                std::thread::spawn(move || {
                    let result = std::process::Command::new("wl-copy")
                        .arg(&url)
                        .status()
                        .or_else(|_| {
                            std::process::Command::new("xclip")
                                .args(["-selection", "clipboard"])
                                .stdin(std::process::Stdio::piped())
                                .spawn()
                                .and_then(|mut child| {
                                    use std::io::Write;
                                    if let Some(stdin) = child.stdin.as_mut() {
                                        stdin.write_all(url.as_bytes())?;
                                    }
                                    child.wait()
                                })
                        });
                    if let Err(e) = result {
                        tracing::error!("Failed to copy username link: {}", e);
                    }
                });
                w.global::<crate::ModalHostState>()
                    .set_profile_share_status("Link copied to clipboard".into());
            }
        });
}

/// Create (or rotate) the username link in the background and push URL + QR
/// into the modal. `rotate=false` reuses an existing local link if present.

fn spawn_create_link(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, rotate: bool) {
    tokio::spawn(async move {
        // First press with an existing link: just re-show it.
        if !rotate {
            if let Ok(Some(url)) = ui_ctx.username_link_local() {
                let ui_ctx2 = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        set_share_link_props(&w, &ui_ctx2, &url);
                        w.global::<crate::ModalHostState>()
                            .set_profile_share_status("".into());
                    }
                });
                return;
            }
        }

        match ui_ctx.create_username_link().await {
            Ok(url) => {
                let status = if rotate {
                    "Link reset — previously shared links no longer work"
                } else {
                    ""
                };
                let ui_ctx2 = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        set_share_link_props(&w, &ui_ctx2, &url);
                        w.global::<crate::ModalHostState>()
                            .set_profile_share_status(status.into());
                    }
                });
            }
            Err(e) => {
                tracing::error!("Failed to create username link: {}", e);
                // Never the Display chain ("invalid input: ...") — that is for logs.
                let msg = e.user_message();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::ModalHostState>()
                            .set_profile_share_status(msg.into());
                    }
                });
            }
        }
    });
}

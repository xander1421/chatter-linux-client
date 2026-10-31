//! Remote settings remain usable while local model controls are absent.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { AISettings } from "../../ui/settings/ai_settings.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { JarvisGlobal } from "../../ui/jarvis/jarvis_global.slint";
    export component RemoteSettingsHarness inherits Window {
        width: 360px;
        height: 800px;
        in-out property <bool> remote: false;
        in-out property <string> mode: "direct-anthropic";
        callback theme(string);
        theme(value) => { Appearance.theme = value; JarvisGlobal.enabled = true; }
        AISettings {
            x: 16px; y: 16px;
            width: parent.width - 32px;
            mobile-platform: true;
            ai-prefer-cloud <=> root.remote;
            cloud-ai-mode <=> root.mode;
        }
    }
}

#[test]
fn remote_settings_keep_consent_and_provider_controls_without_local_models() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for width in [360.0, 960.0] {
        for theme in ["light", "dark"] {
            let window = RemoteSettingsHarness::new().unwrap();
            window
                .window()
                .set_size(slint::LogicalSize::new(width, 800.0));
            window.invoke_theme(theme.into());
            window.show().unwrap();
            let visible = |label| {
                testing::ElementHandle::find_by_accessible_label(&window, label)
                    .any(|e| e.size().width > 0.0 && e.size().height > 0.0)
            };
            let _ = window.window().take_snapshot().unwrap();
            assert!(visible("Use cloud AI"));
            assert!(!visible("Enable local AI"));
            assert!(!visible("Save key"));
            window.set_remote(true);
            let _ = window.window().take_snapshot().unwrap();
            assert!(visible("My API key"));
            assert!(visible("Buy tokens"));
            assert!(visible("Save key"));
            assert!(visible("Assistant memory"));
            window.set_mode("server-broker".into());
            let _ = window.window().take_snapshot().unwrap();
            assert!(visible("Refresh balance"));
            assert!(!visible("Save key"));
            assert!(!visible("Enable local AI"));
        }
    }
}

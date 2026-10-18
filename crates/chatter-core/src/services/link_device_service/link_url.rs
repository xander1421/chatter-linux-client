use super::*;

/// Render a `chatter://link?…` URL as a square PNG QR code.
///
/// `size_px` is clamped to `[128, 1024]`. Returned bytes are a complete
/// PNG image suitable for `slint::Image::load_from_memory` or saving to disk.
pub fn qr_png_for_link_url(url: &str, size_px: u32) -> Result<Vec<u8>, LinkDeviceError> {
    use image::{ImageEncoder, Luma};
    use qrcode::QrCode;

    let size = size_px.clamp(128, 1024);
    let qr = QrCode::new(url.as_bytes())
        .map_err(|e| LinkDeviceError::Invalid(format!("QR generate: {e}")))?;
    let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();
    let scaled =
        image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
        .map_err(|e| LinkDeviceError::Invalid(format!("PNG encode: {e}")))?;
    Ok(png)
}

// ============================================================================
// Helpers
// ============================================================================

pub(super) fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(super) fn base64_url_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.decode(s)
}

pub(super) fn build_link_url(
    uuid: &str,
    pub_key_b64: &str,
    server_url: Option<&str>,
    device_name: Option<&str>,
) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("uuid", uuid);
    query.append_pair("pub_key", pub_key_b64);
    if let Some(server) = server_url {
        query.append_pair("server", server);
    }
    if let Some(name) = device_name {
        query.append_pair("name", name);
    }
    format!("{LINK_URL_SCHEME}://{LINK_URL_HOST}?{}", query.finish())
}

pub(super) fn random_provisioning_code() -> String {
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("OS CSPRNG must not fail");
    base64_url(&bytes)
}

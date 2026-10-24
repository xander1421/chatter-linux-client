//! Verification boundary for executable client updates.
//!
//! Nothing in this module installs code. Callers must verify the complete
//! artifact before handing it to a platform installer.

use crate::network::grpc::client_pool::{GrpcClientPool, TransportMode};
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DOMAIN: &[u8] = b"chatter-release-update-v1\0";
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

pub fn installed_version_code() -> u64 {
    #[cfg(target_os = "android")]
    let value = option_env!("CHATTER_ANDROID_VERSION_CODE");
    #[cfg(not(target_os = "android"))]
    let value = option_env!("CHATTER_LINUX_VERSION_CODE");
    value.and_then(|text| text.parse().ok()).unwrap_or(1)
}

pub fn installed_version_name() -> &'static str {
    #[cfg(target_os = "android")]
    {
        option_env!("CHATTER_ANDROID_VERSION_NAME").unwrap_or("0.1.0")
    }
    #[cfg(not(target_os = "android"))]
    {
        option_env!("CHATTER_LINUX_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
    }
}

pub fn channel() -> &'static str {
    #[cfg(target_os = "android")]
    {
        if option_env!("CHATTER_ANDROID_DISTRIBUTION") == Some("play") {
            "play"
        } else {
            "sideload"
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        "stable"
    }
}

pub fn platform() -> &'static str {
    #[cfg(target_os = "android")]
    {
        "android"
    }
    #[cfg(not(target_os = "android"))]
    {
        "linux"
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    pub platform: String,
    pub channel: String,
    pub version_code: u64,
    pub version_name: String,
    pub min_supported: u64,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub release_notes: String,
    pub published_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifest {
    pub manifest: UpdateManifest,
    pub signature: String,
}

pub fn signed_bytes(manifest: &UpdateManifest) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(manifest).map_err(|_| "cannot encode update manifest")?;
    let mut bytes = Vec::with_capacity(DOMAIN.len() + json.len());
    bytes.extend_from_slice(DOMAIN);
    bytes.extend_from_slice(&json);
    Ok(bytes)
}

/// Verify against an explicit key so tests can check wrong-key rejection.
/// Production callers pass `config::RELEASE_UPDATE_PUBLIC_KEY_BYTES`.
pub fn verify_manifest(
    bytes: &[u8],
    public_key: &[u8; 32],
    expected_platform: &str,
    expected_channel: &str,
    installed_code: u64,
) -> Result<UpdateManifest, String> {
    verify_manifest_inner(
        bytes,
        public_key,
        expected_platform,
        expected_channel,
        installed_code,
        true,
    )
}

fn verify_manifest_inner(
    bytes: &[u8],
    public_key: &[u8; 32],
    expected_platform: &str,
    expected_channel: &str,
    installed_code: u64,
    require_newer: bool,
) -> Result<UpdateManifest, String> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err("update manifest is too large".into());
    }
    let signed: SignedManifest =
        serde_json::from_slice(bytes).map_err(|_| "update manifest is malformed".to_string())?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature)
        .map_err(|_| "update signature is malformed".to_string())?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| "update signature is malformed".to_string())?;
    let key = VerifyingKey::from_bytes(public_key)
        .map_err(|_| "update signing key is invalid".to_string())?;
    key.verify(&signed_bytes(&signed.manifest)?, &signature)
        .map_err(|_| "update signature is invalid".to_string())?;
    let manifest = signed.manifest;
    if manifest.platform != expected_platform || manifest.channel != expected_channel {
        return Err("update targets another platform or channel".into());
    }
    if require_newer && manifest.version_code <= installed_code {
        return Err("update version is not newer than installed version".into());
    }
    if installed_code < manifest.min_supported {
        return Err("installed version is too old for this update".into());
    }
    if manifest.size == 0 || manifest.size > MAX_ARTIFACT_BYTES {
        return Err("update artifact size is invalid".into());
    }
    if manifest.version_name.is_empty()
        || manifest.version_name.len() > 64
        || manifest.release_notes.len() > 4096
        || manifest.published_at.len() > 64
    {
        return Err("update metadata is invalid".into());
    }
    let digest = hex::decode(&manifest.sha256)
        .map_err(|_| "update artifact digest is malformed".to_string())?;
    if digest.len() != 32 || manifest.sha256 != manifest.sha256.to_ascii_lowercase() {
        return Err("update artifact digest is malformed".into());
    }
    Ok(manifest)
}

/// Hash the staged file with a bounded reader. A size mismatch is fatal even
/// if a prefix happens to have the expected digest.
pub fn verify_artifact(mut reader: impl Read, manifest: &UpdateManifest) -> Result<(), String> {
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|_| "update artifact read failed")?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or("update artifact too large")?;
        if total > manifest.size {
            return Err("update artifact size mismatch".into());
        }
        hasher.update(&chunk[..n]);
    }
    if total != manifest.size {
        return Err("update artifact size mismatch".into());
    }
    let actual = hasher.finalize();
    let expected =
        hex::decode(&manifest.sha256).map_err(|_| "update artifact digest is malformed")?;
    if actual.as_slice() != expected.as_slice() {
        return Err("update artifact SHA-256 mismatch".into());
    }
    Ok(())
}

#[cfg(target_os = "android")]
pub fn android_handoff_version_code(
    path: &Path,
    manifest: &UpdateManifest,
) -> Result<i64, String> {
    if manifest.platform != "android"
        || path.file_name().and_then(|name| name.to_str())
            != Some(format!("android-{}.download", manifest.version_code).as_str())
    {
        return Err("verified Android update does not match the downloaded artifact".into());
    }
    i64::try_from(manifest.version_code).map_err(|_| "Android update version code is invalid".into())
}

fn update_url(origin: &str, path: &str) -> Result<String, String> {
    let base = url::Url::parse(origin).map_err(|_| "update server URL is invalid")?;
    if base.scheme() != "https"
        || base.host_str().is_none()
        || base.username() != ""
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || base.path() != "/"
    {
        return Err("updates require the configured HTTPS server origin".into());
    }
    Ok(format!("{}{}", base.as_str().trim_end_matches('/'), path))
}

fn validate_artifact_url(origin: &str, manifest: &UpdateManifest) -> Result<(), String> {
    let expected_prefix = update_url(
        origin,
        &format!("/updates/v1/{}/{}/", manifest.platform, manifest.channel),
    )?;
    let artifact = url::Url::parse(&manifest.url).map_err(|_| "update artifact URL is invalid")?;
    if artifact.scheme() != "https"
        || artifact.username() != ""
        || artifact.password().is_some()
        || artifact.query().is_some()
        || artifact.fragment().is_some()
        || !manifest.url.starts_with(&expected_prefix)
    {
        return Err("update artifact must use the configured HTTPS update origin".into());
    }
    let name = &manifest.url[expected_prefix.len()..];
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || name.contains("..")
    {
        return Err("update artifact path is invalid".into());
    }
    Ok(())
}

/// Check via the same pinned CA as storage, or the existing Tor+TLS path while
/// Shield is active. The caller passes the live transport state, not a cached
/// preference, so a failed Shield switch never silently falls back to direct.
pub async fn check_latest(
    pool: &GrpcClientPool,
    shield_requested: bool,
    platform: &str,
    channel: &str,
    installed_code: u64,
) -> Result<Option<UpdateManifest>, String> {
    tracing::info!(platform, channel, installed_code, "update check started");
    if shield_requested && pool.transport_mode() != TransportMode::Onion {
        return Err("Shield requires Tor for update checks".into());
    }
    let mode = pool.transport_mode();
    let origin = pool.update_origin();
    let url = update_url(
        &origin,
        &format!("/updates/v1/{platform}/{channel}/latest.json"),
    )?;
    let bytes = if mode == TransportMode::Onion {
        #[cfg(feature = "onion")]
        {
            crate::network::anon_http::get_optional(&pool.onion_tor(), &url).await?
        }
        #[cfg(not(feature = "onion"))]
        {
            return Err("Tor support is missing".into());
        }
    } else {
        let mut response = crate::network::storage_http::storage_client()?
            .get(&url)
            .send()
            .await
            .map_err(|_| "update check failed")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!("update check returned {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_MANIFEST_BYTES as u64)
        {
            return Err("update manifest is too large".into());
        }
        let mut body = Vec::new();
        while let Some(chunk) =
            tokio::time::timeout(std::time::Duration::from_secs(30), response.chunk())
                .await
                .map_err(|_| "update check stalled")?
                .map_err(|_| "update manifest read failed")?
        {
            if body
                .len()
                .checked_add(chunk.len())
                .is_none_or(|n| n > MAX_MANIFEST_BYTES)
            {
                return Err("update manifest is too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        Some(body)
    };
    if pool.transport_mode() != mode {
        return Err("network mode changed during update check".into());
    }
    let Some(bytes) = bytes else {
        tracing::info!("update check found no release");
        return Ok(None);
    };
    let manifest = verify_manifest_inner(
        &bytes,
        &crate::config::RELEASE_UPDATE_PUBLIC_KEY_BYTES,
        platform,
        channel,
        installed_code,
        false,
    )?;
    if manifest.version_code <= installed_code {
        tracing::info!("update check found no newer release");
        return Ok(None);
    }
    validate_artifact_url(&origin, &manifest)?;
    tracing::info!(version = %manifest.version_name, size = manifest.size, "update available");
    Ok(Some(manifest))
}

/// Download to a private staging file and verify before returning its path.
/// Only ChatterClient's signature-checked pending manifest may enter here.
pub(crate) async fn download_verified_with_progress(
    pool: &GrpcClientPool,
    shield_requested: bool,
    manifest: &UpdateManifest,
    data_dir: &Path,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<PathBuf, String> {
    let origin_host = url::Url::parse(&manifest.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    tracing::info!(version = %manifest.version_name, size = manifest.size, origin_host, "update download started");
    if shield_requested && pool.transport_mode() != TransportMode::Onion {
        return Err("Shield requires Tor for update downloads".into());
    }
    let mode = pool.transport_mode();
    validate_artifact_url(&pool.update_origin(), manifest)?;
    let folder = data_dir.join("updates");
    tokio::fs::create_dir_all(&folder)
        .await
        .map_err(|_| "cannot create update staging directory")?;
    let path = folder.join(format!(
        "{}-{}.download",
        manifest.platform, manifest.version_code
    ));
    // Keep a partial only when its length is plausible. The final digest is
    // always checked over the entire file before this path is returned.
    let partial = tokio::fs::metadata(&path)
        .await
        .ok()
        .map(|meta| meta.len())
        .unwrap_or(0);
    let resume = mode != TransportMode::Onion && partial > 0 && partial < manifest.size;
    if !resume {
        let _ = tokio::fs::remove_file(&path).await;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(resume)
        .truncate(!resume)
        .open(&path)
        .await
        .map_err(|_| "cannot create update staging file")?;
    let logged_percent = AtomicU64::new(0);
    let report_progress = |bytes: u64| {
        progress(bytes);
        let step = (bytes.saturating_mul(100) / manifest.size) / 10;
        if step > logged_percent.fetch_max(step, Ordering::Relaxed) {
            tracing::info!(percent = step * 10, "update download progress");
        }
    };
    let result = async {
        if mode == TransportMode::Onion {
            #[cfg(feature = "onion")]
            {
                crate::network::anon_http::get_to_file_with_progress(
                    &pool.onion_tor(),
                    &manifest.url,
                    &mut file,
                    manifest.size,
                    &report_progress,
                )
                .await?;
            }
            #[cfg(not(feature = "onion"))]
            {
                return Err("Tor support is missing".into());
            }
        } else {
            let client = crate::network::storage_http::storage_client()?;
            let request = if resume {
                client
                    .get(&manifest.url)
                    .header(reqwest::header::RANGE, format!("bytes={partial}-"))
            } else {
                client.get(&manifest.url)
            };
            let mut response =
                tokio::time::timeout(std::time::Duration::from_secs(30), request.send())
                    .await
                    .map_err(|_| "update download stalled")?
                    .map_err(|_| "update download failed")?;
            let mut total = if resume { partial } else { 0 };
            if resume {
                let expected = format!("bytes {partial}-{}/{}", manifest.size - 1, manifest.size);
                if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
                    || response
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        != Some(expected.as_str())
                {
                    tracing::info!(
                        "update server did not support safe resume; restarting download"
                    );
                    file.set_len(0)
                        .await
                        .map_err(|_| "cannot reset update staging file")?;
                    file = tokio::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .await
                        .map_err(|_| "cannot reopen update staging file")?;
                    response = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        client.get(&manifest.url).send(),
                    )
                    .await
                    .map_err(|_| "update download stalled")?
                    .map_err(|_| "update download failed")?;
                    total = 0;
                } else {
                    tracing::info!(bytes = partial, "update download resuming");
                    report_progress(partial);
                }
            }
            if !response.status().is_success() {
                return Err(format!("update download returned {}", response.status()));
            }
            while let Some(chunk) =
                tokio::time::timeout(std::time::Duration::from_secs(30), response.chunk())
                    .await
                    .map_err(|_| "update download stalled")?
                    .map_err(|_| "update download read failed")?
            {
                total = crate::network::anon_http::write_storage_chunk(
                    &mut file,
                    total,
                    &chunk,
                    manifest.size,
                )
                .await?;
                report_progress(total);
            }
        }
        if pool.transport_mode() != mode {
            return Err("network mode changed during update download".into());
        }
        file.sync_all()
            .await
            .map_err(|_| "update staging sync failed")?;
        let path_for_verify = path.clone();
        let manifest_for_verify = manifest.clone();
        tokio::task::spawn_blocking(move || {
            let input =
                std::fs::File::open(path_for_verify).map_err(|_| "update staging read failed")?;
            verify_artifact(input, &manifest_for_verify)
        })
        .await
        .map_err(|_| "update verification task failed")?
    }
    .await;
    drop(file);
    if let Err(error) = result {
        tracing::warn!(kind = %error, "update download failed");
        if error.contains("size") || error.contains("SHA-256") || error.contains("network mode") {
            let _ = tokio::fs::remove_file(&path).await;
        }
        return Err(error);
    }
    tracing::info!(version = %manifest.version_name, size = manifest.size, "update verified: size and SHA-256 match");
    Ok(path)
}

/// Expand a verified Linux system-runtime archive into a versioned private
/// directory. The previous version is never removed, giving a rollback path.
#[cfg(target_os = "linux")]
pub fn stage_linux_archive(
    archive_path: &Path,
    manifest: &UpdateManifest,
    data_dir: &Path,
) -> Result<PathBuf, String> {
    use std::path::Component;
    if manifest.platform != "linux" {
        return Err("update is not a Linux bundle".into());
    }
    let input = std::fs::File::open(archive_path).map_err(|_| "update archive cannot be opened")?;
    verify_artifact(input, manifest)?;
    let input =
        std::fs::File::open(archive_path).map_err(|_| "update archive cannot be reopened")?;
    let versions = data_dir.join("updates").join("versions");
    std::fs::create_dir_all(&versions).map_err(|_| "cannot create update versions directory")?;
    let final_dir = versions.join(manifest.version_code.to_string());
    if final_dir.exists() {
        return Err("update version is already staged".into());
    }
    let stage = versions.join(format!("{}.staging", manifest.version_code));
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(|_| "stale update staging cannot be removed")?;
    }
    std::fs::create_dir(&stage).map_err(|_| "cannot create update archive staging")?;
    let outcome = (|| {
        let decoder = flate2::read::GzDecoder::new(input);
        let mut archive = tar::Archive::new(decoder);
        let mut saw_binary = false;
        let mut saw_build = false;
        let mut extracted = 0u64;
        for entry in archive.entries().map_err(|_| "update tar is invalid")? {
            let mut entry = entry.map_err(|_| "update tar entry is invalid")?;
            let path = entry.path().map_err(|_| "update tar path is invalid")?;
            let mut components = path.components();
            let root = components.next().ok_or("update tar path is empty")?;
            let Component::Normal(root_name) = root else {
                return Err("update tar path is unsafe".into());
            };
            if !root_name.to_string_lossy().starts_with("chatter-") {
                return Err("update tar root is unexpected".into());
            }
            let mut relative = PathBuf::new();
            for component in components {
                match component {
                    Component::Normal(name) => relative.push(name),
                    _ => return Err("update tar path is unsafe".into()),
                }
            }
            if relative.as_os_str().is_empty() {
                continue;
            }
            let ty = entry.header().entry_type();
            if !ty.is_file() && !ty.is_dir() {
                return Err("update tar contains a link or special file".into());
            }
            if relative == Path::new("BUILD.txt") && entry.size() > 4096 {
                return Err("update build metadata is too large".into());
            }
            extracted = extracted
                .checked_add(entry.size())
                .ok_or("update archive expands too far")?;
            if extracted > 1024 * 1024 * 1024 {
                return Err("update archive expands too far".into());
            }
            let dest = stage.join(&relative);
            if ty.is_dir() {
                std::fs::create_dir_all(&dest).map_err(|_| "update archive directory failed")?;
            } else {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|_| "update archive directory failed")?;
                }
                let mut output = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .map_err(|_| "duplicate or invalid update archive file")?;
                std::io::copy(&mut entry, &mut output)
                    .map_err(|_| "update archive extraction failed")?;
                output
                    .sync_all()
                    .map_err(|_| "update archive sync failed")?;
                if relative == Path::new("bin/chatter-ui") {
                    saw_binary = true;
                } else if relative == Path::new("BUILD.txt") {
                    saw_build = true;
                }
            }
        }
        if !saw_binary {
            return Err("update archive has no client executable".into());
        }
        if !saw_build {
            return Err("update archive has no build metadata".into());
        }
        let build = std::fs::read_to_string(stage.join("BUILD.txt"))
            .map_err(|_| "update build metadata is invalid")?;
        let fields: Vec<_> = build.lines().collect();
        let expected_name = format!("version={}", manifest.version_name);
        let expected_code = format!("version_code={}", manifest.version_code);
        if fields.iter().filter(|line| line.starts_with("version=")).count() != 1
            || fields.iter().filter(|line| line.starts_with("version_code=")).count() != 1
            || !fields.contains(&expected_name.as_str())
            || !fields.contains(&expected_code.as_str())
        {
            return Err("update build version differs from signed manifest".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let binary = stage.join("bin/chatter-ui");
            std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "update executable permissions failed")?;
        }
        let runtime_check = stage.join("check-linux-runtime.sh");
        if !runtime_check.is_file() {
            return Err("update archive has no runtime checker".into());
        }
        let status = std::process::Command::new("/bin/bash")
            .arg(&runtime_check)
            .arg(stage.join("bin/chatter-ui"))
            .status()
            .map_err(|_| "update runtime check could not start")?;
        if !status.success() {
            return Err("update runtime requirements are missing".into());
        }
        std::fs::rename(&stage, &final_dir).map_err(|_| "cannot commit update version")?;
        Ok(final_dir.join("bin/chatter-ui"))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    outcome
}

#[cfg(target_os = "linux")]
struct LinuxLauncherRollback {
    launcher: PathBuf,
    previous: String,
    activated: String,
}

#[cfg(target_os = "linux")]
impl LinuxLauncherRollback {
    fn restore(self) -> Result<(), String> {
        let current = std::fs::read_to_string(&self.launcher)
            .map_err(|_| "cannot read Linux desktop launcher for rollback")?;
        if current != self.activated {
            return Ok(());
        }
        let temporary = self.launcher.with_extension("desktop.rollback-pending");
        std::fs::write(&temporary, self.previous)
            .map_err(|_| "cannot write Linux desktop launcher rollback")?;
        std::fs::set_permissions(
            &temporary,
            std::fs::metadata(&self.launcher)
                .map_err(|_| "cannot read Linux launcher permissions for rollback")?
                .permissions(),
        )
        .map_err(|_| "cannot preserve Linux launcher rollback permissions")?;
        std::fs::rename(temporary, self.launcher)
            .map_err(|_| "cannot restore Linux desktop launcher")?;
        Ok(())
    }
}

/// Point an existing per-user desktop launcher at the verified staged binary.
/// Keep the first launcher as a manual rollback path to the previous executable.
#[cfg(target_os = "linux")]
fn activate_linux_launcher(executable: &Path) -> Result<Option<LinuxLauncherRollback>, String> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("cannot locate Linux desktop launcher")?;
    let launcher = data_home.join("applications/chatter-linux.desktop");
    if !launcher.is_file() {
        return Ok(None);
    }
    let current = std::env::current_exe().map_err(|_| "cannot locate running executable")?;
    let old_exec = format!("Exec={}", current.display());
    let contents =
        std::fs::read_to_string(&launcher).map_err(|_| "cannot read Linux desktop launcher")?;
    if !contents
        .lines()
        .any(|line| line == old_exec || line == "Exec=chatter-ui")
    {
        return Ok(None);
    }
    let new_exec = format!("Exec={}", executable.display());
    let updated = contents
        .lines()
        .map(|line| {
            if line == old_exec || line == "Exec=chatter-ui" {
                new_exec.as_str()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let backup = launcher.with_extension("desktop.rollback");
    if !backup.exists() {
        std::fs::copy(&launcher, &backup).map_err(|_| "cannot save Linux launcher rollback")?;
    }
    let temporary = launcher.with_extension("desktop.update-pending");
    std::fs::write(&temporary, &updated).map_err(|_| "cannot write Linux desktop launcher")?;
    std::fs::set_permissions(
        &temporary,
        std::fs::metadata(&launcher)
            .map_err(|_| "cannot read Linux launcher permissions")?
            .permissions(),
    )
    .map_err(|_| "cannot preserve Linux launcher permissions")?;
    std::fs::rename(temporary, &launcher)
        .map_err(|_| "cannot activate Linux desktop launcher")?;
    tracing::info!(
        "Linux desktop launcher points at verified update; original launcher saved for rollback"
    );
    Ok(Some(LinuxLauncherRollback {
        launcher,
        previous: contents,
        activated: updated,
    }))
}

/// Activate the launcher, then start the staged binary. Restore the immediately
/// prior launcher if the binary cannot start and the launcher is still ours.
#[cfg(target_os = "linux")]
pub fn restart_into_linux_update(executable: &Path) -> Result<(), String> {
    let rollback = activate_linux_launcher(executable)?;
    if std::process::Command::new(executable).spawn().is_err() {
        if let Some(rollback) = rollback {
            rollback.restore()?;
        }
        return Err("Could not restart into the verified update".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn fixture() -> (UpdateManifest, SigningKey) {
        let artifact = b"test apk";
        (
            UpdateManifest {
                platform: "android".into(),
                channel: "sideload".into(),
                version_code: 122,
                version_name: "0.1.0-rc122".into(),
                min_supported: 1,
                url: "https://updates.example/updates/v1/android/sideload/122.apk".into(),
                size: artifact.len() as u64,
                sha256: hex::encode(Sha256::digest(artifact)),
                release_notes: "Fixes".into(),
                published_at: "2026-10-14T00:00:00Z".into(),
            },
            SigningKey::from_bytes(&[27; 32]),
        )
    }
    fn encode(manifest: UpdateManifest, key: &SigningKey) -> Vec<u8> {
        let signature = key.sign(&signed_bytes(&manifest).unwrap());
        serde_json::to_vec(&SignedManifest {
            manifest,
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        })
        .unwrap()
    }
    #[test]
    fn signature_and_target_controls() {
        let (manifest, key) = fixture();
        let bytes = encode(manifest.clone(), &key);
        let public = key.verifying_key().to_bytes();
        assert_eq!(
            verify_manifest(&bytes, &public, "android", "sideload", 121).unwrap(),
            manifest
        );
        assert!(verify_manifest(&bytes, &[1; 32], "android", "sideload", 121).is_err());
        assert!(verify_manifest(&bytes, &public, "linux", "sideload", 121).is_err());
        assert!(verify_manifest(&bytes, &public, "android", "play", 121).is_err());
        assert!(verify_manifest(&bytes, &public, "android", "sideload", 122).is_err());
        assert!(verify_manifest(&bytes, &public, "android", "sideload", 123).is_err());
        assert!(verify_manifest_inner(&bytes, &public, "android", "sideload", 122, false).is_ok());
        let mut tampered: SignedManifest = serde_json::from_slice(&bytes).unwrap();
        tampered.manifest.url.push_str("?injected=1");
        assert!(
            verify_manifest(
                &serde_json::to_vec(&tampered).unwrap(),
                &public,
                "android",
                "sideload",
                121
            )
            .is_err()
        );
    }
    #[test]
    fn artifact_integrity() {
        let (manifest, _) = fixture();
        verify_artifact(&b"test apk"[..], &manifest).unwrap();
        assert!(verify_artifact(&b"test apx"[..], &manifest).is_err());
        assert!(verify_artifact(&b"test ap"[..], &manifest).is_err());
        assert!(verify_artifact(&b"test apk!"[..], &manifest).is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn publishing_tool_round_trip_verifies_in_client() {
        let temp = tempfile::tempdir().unwrap();
        // The publisher inspects the artifact before signing: a Linux archive
        // must carry a BUILD.txt that matches the requested version.
        let artifact = temp.path().join("fixture.tar.gz");
        {
            let encoder = flate2::write::GzEncoder::new(
                std::fs::File::create(&artifact).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(encoder);
            let build = b"version=0.1.0-rc122\nversion_code=122\ntarget=x86_64-unknown-linux-gnu\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(build.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "chatter-122/BUILD.txt", &build[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/publish-update.py");
        let output = std::process::Command::new("python3")
            .arg(script)
            .args([
                "--test-dev-seed",
                "--platform",
                "linux",
                "--channel",
                "stable",
                "--version-code",
                "122",
                "--version-name",
                "0.1.0-rc122",
                "--release-notes",
                "Test",
                "--origin",
                "https://updates.example",
                "--artifact",
            ])
            .arg(&artifact)
            .arg("--publish-dir")
            .arg(temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "publisher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let signed = std::fs::read(temp.path().join("linux/stable/latest.json")).unwrap();
        let manifest =
            verify_manifest(&signed, &dev_release_key(), "linux", "stable", 121).unwrap();
        verify_artifact(
            std::fs::File::open(temp.path().join("linux/stable/122.tar.gz")).unwrap(),
            &manifest,
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    fn dev_release_key() -> [u8; 32] {
        hex::decode("ca2efc2c4186cdcdf29fabe4f33975dd4672bc2e09469a42595a864fca39795a")
            .unwrap()
            .try_into()
            .unwrap()
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_archive_stages_only_regular_files_and_keeps_previous_version() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("update.tar.gz");
        let output = std::fs::File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(output, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        let bytes = b"new signed executable";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "chatter-122/bin/chatter-ui", &bytes[..])
            .unwrap();
        let checker = b"#!/bin/sh\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(checker.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(
            &mut header,
            "chatter-122/check-linux-runtime.sh",
            &checker[..],
        )
        .unwrap();
        let build = b"version=0.1.0-rc122\nversion_code=122\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(build.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "chatter-122/BUILD.txt", &build[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let contents = std::fs::read(&archive_path).unwrap();
        let mut manifest = fixture().0;
        manifest.platform = "linux".into();
        manifest.size = contents.len() as u64;
        manifest.sha256 = hex::encode(Sha256::digest(&contents));
        let previous = temp.path().join("updates/versions/121/bin");
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::write(previous.join("chatter-ui"), b"old executable").unwrap();
        let staged = stage_linux_archive(&archive_path, &manifest, temp.path()).unwrap();
        assert_eq!(std::fs::read(staged).unwrap(), bytes);
        assert_eq!(
            std::fs::read(previous.join("chatter-ui")).unwrap(),
            b"old executable"
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_archive_rejects_links_before_launch() {
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("linked.tar.gz");
        let output = std::fs::File::create(&archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(output, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("/usr/bin/false").unwrap();
        header.set_cksum();
        tar.append_data(&mut header, "chatter-122/bin/chatter-ui", std::io::empty())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let bytes = std::fs::read(&archive_path).unwrap();
        let mut manifest = fixture().0;
        manifest.platform = "linux".into();
        manifest.size = bytes.len() as u64;
        manifest.sha256 = hex::encode(Sha256::digest(&bytes));
        assert!(stage_linux_archive(&archive_path, &manifest, temp.path()).is_err());
        assert!(!temp.path().join("updates/versions/122").exists());
    }
}

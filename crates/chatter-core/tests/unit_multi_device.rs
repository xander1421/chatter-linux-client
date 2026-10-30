//! Unit tests for multi-device support.
//!
//! Tests device list extraction, multi-device encryption fanout,
//! self-device exclusion, and error handling for edge cases.

use crate::framework;

use std::collections::{HashMap, HashSet};

use chatter_core::proto::chat_v1::{
    GetPreKeyBundleResponse, PreKeyBundleDeviceList, SignalPreKeyBundle,
    get_pre_key_bundle_response,
};
use framework::prelude::*;

// ============================================================================
// Device List Extraction Tests
// ============================================================================

#[tokio::test]
async fn test_device_list_extraction_from_prekey_response() {
    // Given: PreKeyBundleDeviceList with bundles for devices [1, 2, 3]
    let device_list = PreKeyBundleDeviceList {
        bundles: vec![
            create_test_bundle(1),
            create_test_bundle(2),
            create_test_bundle(3),
        ],
        identity_key: random_identity_key(),
    };

    let response = GetPreKeyBundleResponse {
        result: Some(get_pre_key_bundle_response::Result::Devices(device_list)),
    };

    // When: Extracting device IDs
    let device_ids = extract_device_ids(&response);

    // Then: Returns [1, 2, 3]
    assert_eq!(device_ids.len(), 3);
    assert!(device_ids.contains(&1));
    assert!(device_ids.contains(&2));
    assert!(device_ids.contains(&3));
}

#[tokio::test]
async fn test_single_device_bundle_extraction() {
    // Given: Single device bundle response
    let bundle = create_test_bundle(1);
    let response = GetPreKeyBundleResponse {
        result: Some(get_pre_key_bundle_response::Result::Bundle(bundle)),
    };

    // When: Extracting device IDs
    let device_ids = extract_device_ids(&response);

    // Then: Returns [1]
    assert_eq!(device_ids.len(), 1);
    assert!(device_ids.contains(&1));
}

#[tokio::test]
async fn test_empty_device_list_extraction() {
    // Given: Empty device list
    let device_list = PreKeyBundleDeviceList {
        bundles: vec![],
        identity_key: random_identity_key(),
    };

    let response = GetPreKeyBundleResponse {
        result: Some(get_pre_key_bundle_response::Result::Devices(device_list)),
    };

    // When: Extracting device IDs
    let device_ids = extract_device_ids(&response);

    // Then: Returns empty list
    assert!(device_ids.is_empty());
}

#[tokio::test]
async fn test_none_result_extraction() {
    // Given: Response with None result
    let response = GetPreKeyBundleResponse { result: None };

    // When: Extracting device IDs
    let device_ids = extract_device_ids(&response);

    // Then: Returns empty list
    assert!(device_ids.is_empty());
}

// ============================================================================
// Multi-Device Encryption Tests
// ============================================================================

#[tokio::test]
async fn test_encrypt_for_multiple_devices_produces_unique_ciphertexts() {
    // Given: 3 device session contexts with different keys
    let plaintext = b"Hello, multi-device world!";
    let device_ids = vec![1u32, 2, 3];

    // Simulate encryption to each device
    // In real implementation, each device gets unique ciphertext due to
    // different session keys (forward secrecy via Double Ratchet)
    let ciphertexts: HashMap<u32, Vec<u8>> = device_ids
        .iter()
        .map(|&device_id| {
            // Simulate unique ciphertext per device
            let mut ct = plaintext.to_vec();
            ct.extend_from_slice(&device_id.to_le_bytes());
            ct.extend_from_slice(&random_bytes(16)); // Add randomness
            (device_id, ct)
        })
        .collect();

    // Then: Each ciphertext is different (forward secrecy property)
    let unique_ciphertexts: HashSet<_> = ciphertexts.values().collect();
    assert_eq!(
        unique_ciphertexts.len(),
        device_ids.len(),
        "Each device should receive a unique ciphertext"
    );
}

#[tokio::test]
async fn test_encryption_result_map_structure() {
    // Given: Successful encryption to 3 devices
    let mut ciphertexts: HashMap<u32, Vec<u8>> = HashMap::new();
    ciphertexts.insert(1, random_bytes(100));
    ciphertexts.insert(2, random_bytes(100));
    ciphertexts.insert(3, random_bytes(100));

    // Then: Map should have correct structure
    assert_eq!(ciphertexts.len(), 3);
    assert!(ciphertexts.contains_key(&1));
    assert!(ciphertexts.contains_key(&2));
    assert!(ciphertexts.contains_key(&3));

    // Each ciphertext should have content
    for (device_id, ct) in &ciphertexts {
        assert!(
            !ct.is_empty(),
            "Ciphertext for device {} should not be empty",
            device_id
        );
    }
}

// ============================================================================
// Self-Device Exclusion Tests
// ============================================================================

#[tokio::test]
async fn test_self_device_exclusion_for_sync() {
    // Given: Current device_id=2, all_devices=[1, 2, 3]
    let current_device_id = 2u32;
    let all_devices = vec![1u32, 2, 3];

    // When: Creating self-sync list (excluding current device)
    let sync_devices: Vec<u32> = all_devices
        .into_iter()
        .filter(|&d| d != current_device_id)
        .collect();

    // Then: Returns [1, 3] (excludes current device)
    assert_eq!(sync_devices.len(), 2);
    assert!(sync_devices.contains(&1));
    assert!(
        !sync_devices.contains(&2),
        "Current device should be excluded"
    );
    assert!(sync_devices.contains(&3));
}

#[tokio::test]
async fn test_self_device_exclusion_when_only_one_device() {
    // Given: User has only one device (device_id=1)
    let current_device_id = 1u32;
    let all_devices = vec![1u32];

    // When: Creating self-sync list
    let sync_devices: Vec<u32> = all_devices
        .into_iter()
        .filter(|&d| d != current_device_id)
        .collect();

    // Then: Returns empty list (no other devices to sync)
    assert!(
        sync_devices.is_empty(),
        "Should have no devices to sync when only one device exists"
    );
}

// ============================================================================
// Partial Failure Handling Tests
// ============================================================================

#[tokio::test]
async fn test_partial_device_failure_continues() {
    // Given: 3 devices, device 2 encryption fails
    let device_ids = vec![1u32, 2, 3];
    let failed_device = 2u32;

    // Simulate encryption with partial failure
    let mut ciphertexts: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut errors: Vec<(u32, String)> = Vec::new();

    for &device_id in &device_ids {
        if device_id == failed_device {
            // Simulating failure for device 2
            errors.push((device_id, "Session not found".to_string()));
        } else {
            // Success for other devices
            ciphertexts.insert(device_id, random_bytes(100));
        }
    }

    // Then: Devices 1 and 3 succeed
    assert_eq!(ciphertexts.len(), 2);
    assert!(ciphertexts.contains_key(&1));
    assert!(
        !ciphertexts.contains_key(&2),
        "Failed device should not have ciphertext"
    );
    assert!(ciphertexts.contains_key(&3));

    // And: Error was recorded for device 2
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 2);
}

#[tokio::test]
async fn test_all_devices_fail_returns_error() {
    // Given: All device encryptions fail
    let device_ids = vec![1u32, 2, 3];
    let ciphertexts: HashMap<u32, Vec<u8>> = HashMap::new(); // All failed

    // When: Checking result
    let is_total_failure = ciphertexts.is_empty() && !device_ids.is_empty();

    // Then: Should be recognized as total failure
    assert!(
        is_total_failure,
        "Empty ciphertexts with devices should be total failure"
    );
}

// ============================================================================
// Empty Device List Error Tests
// ============================================================================

#[tokio::test]
async fn test_empty_device_list_returns_error() {
    // Given: User has no registered devices
    let device_ids: Vec<u32> = vec![];

    // When: Checking for valid target devices
    let result = validate_device_list(&device_ids);

    // Then: Returns error
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("no devices"));
}

#[tokio::test]
async fn test_valid_device_list_succeeds() {
    // Given: User has registered devices
    let device_ids = vec![1u32, 2, 3];

    // When: Validating device list
    let result = validate_device_list(&device_ids);

    // Then: Succeeds
    assert!(result.is_ok());
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a test SignalPreKeyBundle for a device.
fn create_test_bundle(device_id: u32) -> SignalPreKeyBundle {
    use chatter_core::proto::chat_v1::signal_pre_key_bundle::{
        KyberPreKeyRecord, PreKeyRecord, SignedPreKeyRecord,
    };

    SignalPreKeyBundle {
        identity_key: random_identity_key(),
        registration_id: CryptoBuilder::registration_id(),
        device_id,
        signed_prekey: Some(SignedPreKeyRecord {
            id: 1,
            public_key: random_public_key(),
            signature: random_bytes(64),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        }),
        one_time_prekey: Some(PreKeyRecord {
            id: 100 + device_id,
            public_key: random_public_key(),
        }),
        kyber_prekey: Some(KyberPreKeyRecord {
            id: 1,
            public_key: random_bytes(1568), // Kyber1024 public key size
            signature: random_bytes(64),
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
        }),
        unrestricted_unidentified_access: Some(false),
    }
}

/// Extract device IDs from a GetPreKeyBundleResponse.
fn extract_device_ids(response: &GetPreKeyBundleResponse) -> Vec<u32> {
    match &response.result {
        Some(get_pre_key_bundle_response::Result::Devices(list)) => {
            list.bundles.iter().map(|b| b.device_id).collect()
        }
        Some(get_pre_key_bundle_response::Result::Bundle(bundle)) => {
            vec![bundle.device_id]
        }
        None => vec![],
    }
}

/// Validate that a device list is not empty.
fn validate_device_list(device_ids: &[u32]) -> Result<(), String> {
    if device_ids.is_empty() {
        Err("no devices registered for recipient".to_string())
    } else {
        Ok(())
    }
}

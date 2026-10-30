//! Build-time guard for the `insecure-test-anchors` feature (ANCHOR-TESTING).
//!
//! That feature falls the four pinned trust anchors — the sealed-sender trust
//! root, the TLS root CA, the KT tree-head key and the KT VRF key — back to
//! their publicly-known development values when nothing is provisioned, so
//! `cargo test` runs without `make provision-keys`. It exists for the
//! integration tests in `tests/`, which link this crate as an ordinary
//! dependency with `cfg(test)` unset and so cannot use the `cfg(test)` fallback.
//!
//! A cargo feature is additive and global: anything that enables it for any
//! reason gets it for every target in the build, release included. That is how
//! the previous version of this carve-out (keyed on `testing`, a feature the
//! Makefile composes into ordinary builds) produced release artifacts pinned to
//! keys whose private halves are in this repository.
//!
//! So: refuse it in a release profile. The tests that need it run in dev, and a
//! shippable artifact cannot be built with it at all.

// The pinned tonic plugin has no custom-codec option. Adapt its output at
// build time, keeping checked-in generated files reproducible and untouched.
fn validated_clients() {
    let source = std::path::Path::new("src/proto");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=src/proto");
    for entry in std::fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let mut code = std::fs::read_to_string(&path).unwrap();
        let original = "tonic_prost::ProstCodec::default()";
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".tonic.rs")
        {
            let methods = code.matches("let codec = ").count();
            assert!(
                methods > 0 && methods == code.matches(original).count(),
                "generated tonic codec shape changed; update response-validation adapter"
            );
        }
        while let Some(start) = code.find(original) {
            let rest = &code[start + original.len()..];
            let uri_start = rest.find("\"/").expect("tonic RPC path follows codec") + 1;
            let uri_end = uri_start + rest[uri_start..].find('"').unwrap();
            let replacement = format!(
                "crate::network::grpc::response_codec::ResponseCodec::new({:?})",
                &rest[uri_start..uri_end]
            );
            code.replace_range(start..start + original.len(), &replacement);
        }
        std::fs::write(out.join(path.file_name().unwrap()), code).unwrap();
    }
}

fn main() {
    validated_clients();
    println!("cargo:rerun-if-changed=build.rs");

    let insecure_anchors = std::env::var_os("CARGO_FEATURE_INSECURE_TEST_ANCHORS").is_some();
    let profile = std::env::var("PROFILE").unwrap_or_default();

    if insecure_anchors && profile == "release" {
        panic!(
            "\n\n\
             chatter-core feature `insecure-test-anchors` is enabled in a RELEASE profile.\n\n\
             It pins the four trust anchors to their PUBLICLY-KNOWN development values \
             when nothing is provisioned: the sealed-sender trust root (private half \
             committed in deploy/vault/init-and-unseal.sh), the dev TLS root CA, and the \
             KT tree-head and VRF keys (both derived from seed phrases in source). An \
             artifact built this way trusts sender certificates, signed tree heads, VRF \
             leaf indices and TLS certificates that anyone with this repository can forge.\n\n\
             It exists so `cargo test` runs without provisioning, and the tests run in a \
             dev profile.\n\n\
             If you are running tests:      drop --release.\n\
             If you are building a client:  drop --features insecure-test-anchors and \
             provision the anchors with `make provision-keys` (or use --features \
             dev-trust-root plus CHATTER_TLS_ROOT_CA_PEM for a local dev server).\n"
        );
    }
}

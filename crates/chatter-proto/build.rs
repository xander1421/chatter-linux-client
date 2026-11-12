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
                "crate::codec::ResponseCodec::new({:?})",
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
}

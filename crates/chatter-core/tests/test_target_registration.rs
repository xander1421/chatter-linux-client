//! With autodiscovery disabled, forgetting a module would silently drop tests.

#[test]
fn every_test_source_is_registered_once() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let suite = include_str!("it.rs");
    let manifest = include_str!("../Cargo.toml");
    for entry in std::fs::read_dir(tests).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        let module = format!("mod {name};");
        let target = format!("path = \"tests/{name}.rs\"");
        let registrations = suite.lines().filter(|line| *line == module).count()
            + manifest.lines().filter(|line| *line == target).count();
        assert_eq!(registrations, 1, "{name} must be registered exactly once");
    }
}

mod common;

#[test]
fn goldens_load() {
    for name in ["records", "extract", "answered", "mask", "clean"] {
        let cases = common::golden(name);
        assert!(!cases.is_empty(), "{name}.json is empty");
        assert!(
            cases.iter().all(|c| !c.name.is_empty()),
            "{name}: unnamed case"
        );
    }
    let schema = common::golden_value("schema");
    assert_eq!(schema["schema_version_max"], 5);
    assert!(!schema["sqlite_master"].as_array().unwrap().is_empty());
}

#[test]
fn fixture_resolves_into_test_fixtures() {
    let p = common::fixture("replies/full.md");
    assert!(p.is_file(), "{}", p.display());
}

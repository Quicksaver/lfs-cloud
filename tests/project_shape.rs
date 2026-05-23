//! Architecture tests for the MVP package shape and dependency baseline.

use std::path::Path;

use toml::Value;

fn cargo_manifest() -> Value {
    let manifest = std::fs::read_to_string("Cargo.toml").expect("Cargo.toml should be readable");

    toml::from_str::<Value>(&manifest).expect("Cargo.toml should parse as TOML")
}

fn dependency<'a>(manifest: &'a Value, section: &str, name: &str) -> &'a Value {
    manifest
        .get(section)
        .and_then(Value::as_table)
        .and_then(|dependencies| dependencies.get(name))
        .unwrap_or_else(|| panic!("{section}.{name} should be declared"))
}

fn dependency_features(manifest: &Value, section: &str, name: &str) -> Vec<String> {
    dependency(manifest, section, name)
        .get("features")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{section}.{name}.features should be declared"))
        .iter()
        .map(|feature| {
            feature
                .as_str()
                .unwrap_or_else(|| panic!("{section}.{name}.features should be strings"))
                .to_owned()
        })
        .collect()
}

#[test]
fn mvp_uses_single_root_package_with_library_and_binary_targets() {
    let manifest = cargo_manifest();

    assert_eq!(
        manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(Value::as_str),
        Some("lfs-cloud")
    );
    assert!(
        manifest.get("workspace").is_none(),
        "MVP should stay a single root package until module boundaries justify crates"
    );
    assert!(Path::new("src/lib.rs").is_file());
    assert!(Path::new("src/main.rs").is_file());
}

#[test]
fn manifest_declares_planned_baseline_dependencies() {
    let manifest = cargo_manifest();

    for dependency_name in [
        "anyhow",
        "axum",
        "clap",
        "config",
        "oauth2",
        "reqwest",
        "rusqlite",
        "serde",
        "tempfile",
        "thiserror",
        "tokio",
        "tracing",
        "tracing-subscriber",
    ] {
        dependency(&manifest, "dependencies", dependency_name);
    }

    dependency(&manifest, "dev-dependencies", "toml");
}

#[test]
fn dependency_features_match_mvp_decisions() {
    let manifest = cargo_manifest();

    assert!(dependency_features(&manifest, "dependencies", "clap").contains(&"derive".to_owned()));
    assert!(dependency_features(&manifest, "dependencies", "serde").contains(&"derive".to_owned()));
    assert_eq!(
        dependency_features(&manifest, "dependencies", "config"),
        ["yaml"]
    );
    assert!(
        dependency(&manifest, "dependencies", "reqwest")
            .get("default-features")
            .and_then(Value::as_bool)
            .is_some_and(|enabled| !enabled),
        "reqwest should disable native default TLS features"
    );
    assert!(
        dependency_features(&manifest, "dependencies", "reqwest").contains(&"rustls".to_owned())
    );
    assert!(
        dependency(&manifest, "dependencies", "oauth2")
            .get("default-features")
            .and_then(Value::as_bool)
            .is_some_and(|enabled| !enabled),
        "oauth2 should not enable an additional HTTP client stack yet"
    );
}

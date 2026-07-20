//! Architecture tests for the MVP package shape and dependency policy.

use std::{path::Path, sync::OnceLock};

use toml::Value;

static CARGO_MANIFEST: OnceLock<Value> = OnceLock::new();

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn cargo_manifest() -> &'static Value {
    CARGO_MANIFEST.get_or_init(|| {
        let manifest = std::fs::read_to_string(manifest_dir().join("Cargo.toml"))
            .expect("Cargo.toml should be readable");

        toml::from_str::<Value>(&manifest).expect("Cargo.toml should parse as TOML")
    })
}

fn dependency<'a>(manifest: &'a Value, section: &str, name: &str) -> &'a Value {
    manifest
        .get(section)
        .and_then(Value::as_table)
        .and_then(|dependencies| dependencies.get(name))
        .unwrap_or_else(|| panic!("{section}.{name} should be declared"))
}

fn table_dependency<'a>(manifest: &'a Value, section: &str, name: &str) -> &'a toml::Table {
    dependency(manifest, section, name)
        .as_table()
        .unwrap_or_else(|| panic!("{section}.{name} should use a table dependency declaration"))
}

fn dependency_features<'a>(manifest: &'a Value, section: &str, name: &str) -> Vec<&'a str> {
    table_dependency(manifest, section, name)
        .get("features")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{section}.{name}.features should be declared"))
        .iter()
        .map(|feature| {
            feature
                .as_str()
                .unwrap_or_else(|| panic!("{section}.{name}.features should be strings"))
        })
        .collect()
}

fn dependency_has_feature(manifest: &Value, section: &str, name: &str, feature: &str) -> bool {
    dependency_features(manifest, section, name).contains(&feature)
}

fn dependency_disables_default_features(manifest: &Value, section: &str, name: &str) -> bool {
    table_dependency(manifest, section, name)
        .get("default-features")
        .and_then(Value::as_bool)
        .is_some_and(|enabled| !enabled)
}

fn package_flag_is_not_disabled(manifest: &Value, flag: &str) -> bool {
    manifest
        .get("package")
        .and_then(|package| package.get(flag))
        .and_then(Value::as_bool)
        .is_none_or(|enabled| enabled)
}

#[test]
fn mvp_uses_single_root_package_with_library_and_binary_targets() {
    let manifest = cargo_manifest();

    assert_eq!(
        manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(Value::as_str),
        Some("lfscloud")
    );
    assert!(
        manifest.get("workspace").is_none(),
        "MVP should stay a single root package until module boundaries justify crates"
    );
    assert!(
        package_flag_is_not_disabled(manifest, "autolib"),
        "Cargo's default src/lib.rs library target should stay enabled"
    );
    assert!(
        package_flag_is_not_disabled(manifest, "autobins"),
        "Cargo's default src/main.rs binary target should stay enabled"
    );
    assert!(manifest_dir().join("src/lib.rs").is_file());
    assert!(manifest_dir().join("src/main.rs").is_file());
}

#[test]
fn cli_binary_uses_the_unhyphenated_name() {
    let binary_targets = cargo_manifest()
        .get("bin")
        .and_then(Value::as_array)
        .expect("Cargo.toml should declare the CLI binary explicitly");

    assert!(binary_targets.iter().any(|target| {
        target
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name == "lfscloud")
            && target
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path == "src/main.rs")
    }));
}

#[test]
fn dependency_features_match_mvp_decisions() {
    let manifest = cargo_manifest();

    assert!(dependency_has_feature(
        manifest,
        "dependencies",
        "clap",
        "derive"
    ));
    assert!(dependency_has_feature(
        manifest,
        "dependencies",
        "serde",
        "derive"
    ));
    assert_eq!(
        dependency_features(manifest, "dependencies", "config"),
        ["yaml"]
    );
    assert!(
        dependency_disables_default_features(manifest, "dependencies", "config"),
        "config should only enable the selected YAML loader"
    );
    assert!(
        dependency_disables_default_features(manifest, "dependencies", "reqwest"),
        "reqwest should disable native default TLS features"
    );
    assert!(dependency_has_feature(
        manifest,
        "dependencies",
        "reqwest",
        "rustls"
    ));
    assert!(
        dependency_has_feature(manifest, "dependencies", "rusqlite", "bundled"),
        "rusqlite should not require system SQLite development headers"
    );
    assert!(dependency_has_feature(
        manifest,
        "dependencies",
        "tokio",
        "net"
    ));
    assert!(dependency_has_feature(
        manifest,
        "dependencies",
        "tracing-subscriber",
        "env-filter"
    ));
    assert!(dependency_has_feature(
        manifest,
        "dependencies",
        "tracing-subscriber",
        "fmt"
    ));
    assert!(
        dependency_disables_default_features(manifest, "dependencies", "oauth2"),
        "oauth2 should not enable an additional HTTP client stack yet"
    );
}

#![allow(dead_code)]

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct GoldenCase {
    pub name: String,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
}

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

pub fn golden_value(name: &str) -> serde_json::Value {
    let path = goldens_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Cases of `tests/goldens/<name>.json`, produced by `scripts/rust-goldens.ts`.
pub fn golden(name: &str) -> Vec<GoldenCase> {
    serde_json::from_value(golden_value(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// A path under `<workspace>/test/fixtures`.
pub fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test/fixtures")
        .join(rel)
}

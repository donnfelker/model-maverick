//! `conformance.json` lists every frozen scenario and vector file with its sha256 (bead
//! 8.2). A change to a frozen file is a contract change for every SDK, so it must show up
//! in the manifest: this test fails until the manifest is regenerated with
//! `MAVERICK_UPDATE_MANIFEST=1 cargo test -p sdk-conformance --test manifest`. Bump
//! `version` by hand when the contract changes.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn sha256(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn files(dir: &str) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(root().join(dir))
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .map(|entry| {
            let path = entry.expect("entry").path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            (name, path)
        })
        .collect();
    out.sort();
    out
}

fn render(version: &str) -> String {
    let scenarios: Vec<Value> = files("scenarios")
        .into_iter()
        .map(|(name, path)| {
            let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            json!({
                "id": doc["id"],
                "name": doc["name"],
                "group": doc["group"],
                "file": format!("scenarios/{name}"),
                "sha256": sha256(&path),
            })
        })
        .collect();
    let vectors: Vec<Value> = files("vectors")
        .into_iter()
        .map(|(name, path)| {
            // `<set>.<version>.json`.
            let parts: Vec<&str> = name.split('.').collect();
            assert!(
                parts.len() == 3 && parts[2] == "json",
                "{name}: vector files are <set>.<version>.json"
            );
            json!({
                "vector_set": parts[0],
                "version": parts[1],
                "file": format!("vectors/{name}"),
                "sha256": sha256(&path),
            })
        })
        .collect();
    let manifest = json!({
        "contract": "sdk-conformance",
        "version": version,
        "readme": "README.md",
        "scenarios": scenarios,
        "vectors": vectors,
    });
    serde_json::to_string_pretty(&manifest).unwrap() + "\n"
}

#[test]
fn the_manifest_lists_every_frozen_file() {
    let path = root().join("conformance.json");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let version = serde_json::from_str::<Value>(&current)
        .ok()
        .and_then(|doc| doc["version"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "1".to_owned());
    let want = render(&version);
    if std::env::var_os("MAVERICK_UPDATE_MANIFEST").is_some() {
        std::fs::write(&path, &want).expect("write manifest");
        return;
    }
    assert!(
        current == want,
        "crates/sdk-conformance/conformance.json is stale: a frozen scenario or vector \
         changed. That changes the contract every SDK runs. Regenerate it with \
         MAVERICK_UPDATE_MANIFEST=1 cargo test -p sdk-conformance --test manifest, and bump \
         its version."
    );
}

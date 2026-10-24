//! DIVERGENCE: D49. The public bundle is independently authenticated and immutable.
use chrono::{TimeZone, Utc};
use ed25519_dalek::{
    Signer, SigningKey,
    pkcs8::{EncodePrivateKey, spki::der::pem::LineEnding},
};
use maverick_bundle::*;
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    path::PathBuf,
};

fn keys() -> SigningKeys {
    SigningKeys::new(BTreeMap::from([
        ("current".into(), SigningKey::from_bytes(&[7; 32])),
        ("standby".into(), SigningKey::from_bytes(&[9; 32])),
    ]))
    .unwrap()
}
fn manifest(day: u32) -> Manifest {
    Manifest {
        format: FORMAT.into(),
        matrix_version: "public-v1".into(),
        published_at: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
        origin_release: "0.1.0".into(),
        schema: SchemaRange { min: 11, max: 12 },
        files: BTreeMap::new(),
    }
}
fn matrix() -> PublicMatrix {
    PublicMatrix {
        aggregates: vec![Aggregate {
            model_column_id: "column".into(),
            capability: "coding".into(),
            difficulty_tier: 1,
            dimension: "quality".into(),
            ewma: 0.75,
            variance: 0.125,
            n: 4,
            measured: true,
            matrix_version: "public-v1".into(),
            as_of: "2026-10-24".parse().unwrap(),
        }],
        calibration: vec![Calibration {
            batch_id: "global-batch".into(),
            capability: "coding".into(),
            brier_score: 0.25,
            overconfidence: 0.125,
            n: 4,
            scored_at: manifest(24).published_at,
        }],
        checks: BTreeMap::from([(
            "coding".into(),
            vec![serde_json::json!({"type":"checklist","id":"code"})],
        )]),
        ..Default::default()
    }
}
fn entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    tar::Archive::new(Cursor::new(bytes))
        .entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let path = e.path().unwrap().to_string_lossy().into_owned();
            let mut b = Vec::new();
            e.read_to_end(&mut b).unwrap();
            (path, b)
        })
        .collect()
}
fn repack(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in entries {
        let mut h = tar::Header::new_ustar();
        h.set_mode(0o644);
        h.set_size(bytes.len() as u64);
        h.set_cksum();
        tar.append_data(&mut h, path, bytes.as_slice()).unwrap();
    }
    tar.into_inner().unwrap()
}
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "mm-bundle-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn roundtrip_is_deterministic_and_all_four_payloads_are_hashed() {
    let keys = keys();
    let bytes = build(manifest(24), &matrix(), &keys).unwrap();
    assert_eq!(bytes, build(manifest(24), &matrix(), &keys).unwrap());
    let read = verify(&bytes, &keys.public_keys(), 12).unwrap();
    assert_eq!(read.matrix, matrix());
    assert_eq!(read.signing_kids, vec!["current", "standby"]);
    assert_eq!(read.manifest.files.len(), 4);
    for (name, data) in entries(&bytes) {
        if let Some(hash) = read
            .manifest
            .files
            .get(name.strip_prefix("mmbundle1/").unwrap())
        {
            assert_eq!(*hash, sha256(&data));
        }
    }
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
#[test]
fn rotation_accepts_either_known_key_and_requires_a_trusted_valid_signature() {
    let keys = keys();
    let bytes = build(manifest(24), &matrix(), &keys).unwrap();
    for kid in ["current", "standby"] {
        let trust = BTreeMap::from([(kid.to_owned(), keys.public_keys()[kid])]);
        assert_eq!(verify(&bytes, &trust, 11).unwrap().signing_kids, vec![kid]);
    }
    assert!(verify(&bytes, &BTreeMap::new(), 12).is_err());
    assert!(
        verify(
            &bytes,
            &BTreeMap::from([(
                "current".into(),
                SigningKey::from_bytes(&[3; 32]).verifying_key()
            )]),
            12
        )
        .is_err()
    );
    assert!(verify(&bytes, &keys.public_keys(), 10).is_err());
    assert!(verify(&bytes, &keys.public_keys(), 13).is_err());
    let mut e = entries(&bytes);
    e.iter_mut()
        .find(|(n, _)| n.ends_with("signatures/current"))
        .unwrap()
        .1 = vec![0; 64];
    assert_eq!(
        verify(&repack(e), &keys.public_keys(), 12)
            .unwrap()
            .signing_kids,
        vec!["standby"]
    );
}
#[test]
fn every_payload_and_manifest_is_authenticated_and_unknown_members_fail_closed() {
    let keys = keys();
    let bytes = build(manifest(24), &matrix(), &keys).unwrap();
    for target in PAYLOAD_FILES.into_iter().chain(["manifest.json"]) {
        let mut e = entries(&bytes);
        e.iter_mut()
            .find(|(n, _)| n.ends_with(target))
            .unwrap()
            .1
            .push(b' ');
        assert!(
            verify(&repack(e), &keys.public_keys(), 12).is_err(),
            "{target}"
        );
    }
    let e = entries(&bytes);
    for path in [
        "mmbundle1/private.json",
        "elsewhere/catalog.json",
        "mmbundle1/signatures/bad.key",
    ] {
        let mut extra = e.clone();
        extra.push((path.into(), vec![]));
        assert!(verify(&repack(extra), &keys.public_keys(), 12).is_err());
    }
    let mut duplicate = e.clone();
    duplicate.push(e[0].clone());
    assert!(verify(&repack(duplicate), &keys.public_keys(), 12).is_err());
    for target in PAYLOAD_FILES.into_iter().chain(["manifest.json"]) {
        let missing = e
            .iter()
            .filter(|(n, _)| !n.ends_with(target))
            .cloned()
            .collect();
        assert!(verify(&repack(missing), &keys.public_keys(), 12).is_err());
    }
    let mut appended = bytes;
    appended.extend(b"private trailing bytes");
    assert!(verify(&appended, &keys.public_keys(), 12).is_err());
}
#[test]
fn non_regular_entries_are_refused() {
    let mut archive = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    h.set_mode(0o777);
    h.set_link_name("/etc/passwd").unwrap();
    h.set_cksum();
    archive
        .append_data(&mut h, "mmbundle1/catalog.json", &[][..])
        .unwrap();
    assert!(inspect(&archive.into_inner().unwrap()).is_err());
}
#[test]
fn publication_preserves_bytes_on_retry_and_repairs_the_newest_first_index() {
    let dir = Dir::new();
    let keys = keys();
    let first = build(manifest(23), &matrix(), &keys).unwrap();
    let old = publish(&dir.0, &first).unwrap();
    let newer = build(manifest(24), &matrix(), &keys).unwrap();
    let new = publish(&dir.0, &newer).unwrap();
    let mut changed = matrix();
    changed.aggregates[0].ewma = 0.25;
    assert_eq!(
        publish(&dir.0, &build(manifest(24), &changed, &keys).unwrap()).unwrap(),
        new
    );
    assert_eq!(fs::read(dir.0.join(&new.file)).unwrap(), newer);
    fs::write(dir.0.join("index.json"), b"interrupted").unwrap();
    assert_eq!(publish(&dir.0, &first).unwrap(), old);
    let index: Vec<IndexEntry> =
        serde_json::from_slice(&fs::read(dir.0.join("index.json")).unwrap()).unwrap();
    assert_eq!(index, vec![new.clone(), old]);
    assert_eq!(new.sha256, sha256(&newer));
    assert_eq!(new.size, newer.len() as u64);
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.0.join("index.json")).unwrap()).unwrap();
    assert_eq!(value[0].as_object().unwrap().len(), 6);
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3);
}
#[test]
fn runtime_pem_loader_rejects_duplicate_invalid_and_missing_keys_without_disclosure() {
    let dir = Dir::new();
    let path = dir.0.join("runtime.pem");
    let key = SigningKey::from_bytes(&[13; 32]);
    fs::write(&path, key.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let loaded = SigningKeys::from_files(&format!("runtime:{}", path.display())).unwrap();
    assert_eq!(loaded.public_keys()["runtime"], key.verifying_key());
    fs::write(&path, b"PRIVATE-KEY-CANARY").unwrap();
    for setting in [
        format!("runtime:{}", path.display()),
        "kid:/missing/canary".into(),
        "../bad:/missing".into(),
        "".into(),
        format!("a:{0},a:{0}", path.display()),
    ] {
        let err = SigningKeys::from_files(&setting).err().unwrap();
        assert!(!format!("{err:?} {err}").contains("CANARY"));
        assert!(!err.to_string().contains("/missing"));
    }
}
#[test]
fn nonfinite_measurements_cannot_silently_become_json_null() {
    let mut m = matrix();
    m.aggregates[0].ewma = f32::NAN;
    assert!(build(manifest(24), &m, &keys()).is_err());
    let mut m = matrix();
    m.calibration[0].overconfidence = f32::INFINITY;
    assert!(build(manifest(24), &m, &keys()).is_err());
}

#[test]
fn authenticated_measurements_that_overflow_f32_are_refused() {
    let keys = keys();
    let bytes = build(manifest(24), &matrix(), &keys).unwrap();
    for (payload, field) in [
        ("aggregates.jsonl", "ewma"),
        ("aggregates.jsonl", "variance"),
        ("calibration.json", "brier_score"),
        ("calibration.json", "overconfidence"),
    ] {
        for overflow in [-1e100, 1e100] {
            let mut entries = entries(&bytes);
            let path = format!("mmbundle1/{payload}");
            let data = &mut entries.iter_mut().find(|(p, _)| *p == path).unwrap().1;
            let mut value: serde_json::Value = serde_json::from_slice(data).unwrap();
            let row = if payload.ends_with("jsonl") {
                &mut value
            } else {
                &mut value[0]
            };
            row[field] = serde_json::json!(overflow);
            *data = serde_json::to_vec(&value).unwrap();
            let hash = sha256(data);
            let mut manifest = manifest(24);
            manifest.files = inspect(&bytes).unwrap().manifest.files;
            manifest.files.insert(payload.into(), hash);
            let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
            entries
                .iter_mut()
                .find(|(p, _)| p == "mmbundle1/manifest.json")
                .unwrap()
                .1 = manifest_bytes.clone();
            entries
                .iter_mut()
                .find(|(p, _)| p == "mmbundle1/signatures/current")
                .unwrap()
                .1 = SigningKey::from_bytes(&[7; 32])
                .sign(&manifest_bytes)
                .to_bytes()
                .to_vec();
            let archive = repack(entries);
            assert!(matches!(
                inspect(&archive),
                Err(BundleError::Invalid("non-finite measurement"))
            ));
            assert!(
                matches!(
                    verify(&archive, &keys.public_keys(), 12),
                    Err(BundleError::Invalid("non-finite measurement"))
                ),
                "{payload}: {field} = {overflow}"
            );
        }
    }
}

#[test]
fn finite_f32_boundaries_survive_authenticated_roundtrip() {
    let keys = keys();
    let mut matrix = matrix();
    matrix.aggregates[0].ewma = f32::MAX;
    matrix.aggregates[0].variance = f32::MIN_POSITIVE;
    matrix.calibration[0].brier_score = f32::MIN;
    matrix.calibration[0].overconfidence = -0.0;
    let bytes = build(manifest(24), &matrix, &keys).unwrap();
    let read = verify(&bytes, &keys.public_keys(), 12).unwrap();
    assert_eq!(read.matrix, matrix);
    assert!(read.matrix.calibration[0].overconfidence.is_sign_negative());
}

#[test]
fn corrupt_retained_archive_keeps_the_previous_index_and_names_the_bad_file() {
    let dir = Dir::new();
    let first = build(manifest(23), &matrix(), &keys()).unwrap();
    publish(&dir.0, &first).unwrap();
    let index = fs::read(dir.0.join("index.json")).unwrap();
    fs::write(dir.0.join("mmbundle1-corrupt.tar"), b"broken archive").unwrap();
    let error = publish(&dir.0, &build(manifest(24), &matrix(), &keys()).unwrap()).unwrap_err();
    assert!(error.to_string().contains("mmbundle1-corrupt.tar"));
    assert_eq!(fs::read(dir.0.join("index.json")).unwrap(), index);
}

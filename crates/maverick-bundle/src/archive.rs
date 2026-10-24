use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::{FORMAT, Manifest, PAYLOAD_FILES, PublicMatrix};

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("matrix bundle I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("retained matrix bundle {file}: {source}")]
    Retained {
        file: String,
        #[source]
        source: Box<BundleError>,
    },
    #[error("matrix bundle JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("invalid matrix bundle: {0}")]
    Invalid(&'static str),
    // Key/PEM errors never include file contents, paths, or parser diagnostics.
    #[error(
        "matrix signing keys: expected unique kid:path pairs and valid PKCS#8 Ed25519 PEM files"
    )]
    SigningKeys,
}

/// Private material has no Debug or Serialize implementation.
pub struct SigningKeys(BTreeMap<String, SigningKey>);

fn valid_kid(kid: &str) -> bool {
    !kid.is_empty()
        && kid.len() <= 64
        && kid
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

impl SigningKeys {
    pub fn new(keys: BTreeMap<String, SigningKey>) -> Result<Self, BundleError> {
        if keys.is_empty() || keys.keys().any(|kid| !valid_kid(kid)) {
            return Err(BundleError::SigningKeys);
        }
        Ok(Self(keys))
    }

    /// Load only explicit runtime files. No environment-provided key is trusted by a reader.
    pub fn from_files(pairs: &str) -> Result<Self, BundleError> {
        let mut keys = BTreeMap::new();
        for pair in pairs.split(',') {
            let (kid, path) = pair
                .trim()
                .split_once(':')
                .ok_or(BundleError::SigningKeys)?;
            if !valid_kid(kid) || path.is_empty() || keys.contains_key(kid) {
                return Err(BundleError::SigningKeys);
            }
            let pem = std::fs::read_to_string(path).map_err(|_| BundleError::SigningKeys)?;
            let key = SigningKey::from_pkcs8_pem(&pem).map_err(|_| BundleError::SigningKeys)?;
            keys.insert(kid.to_owned(), key);
        }
        Self::new(keys)
    }

    pub fn public_keys(&self) -> BTreeMap<String, VerifyingKey> {
        self.0
            .iter()
            .map(|(kid, key)| (kid.clone(), key.verifying_key()))
            .collect()
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn payloads(matrix: &PublicMatrix) -> Result<BTreeMap<String, Vec<u8>>, BundleError> {
    if matrix
        .aggregates
        .iter()
        .any(|r| !r.ewma.is_finite() || !r.variance.is_finite())
        || matrix
            .calibration
            .iter()
            .any(|r| !r.brier_score.is_finite() || !r.overconfidence.is_finite())
    {
        return Err(BundleError::Invalid("non-finite measurement"));
    }
    let mut matrix = matrix.clone();
    matrix.catalog.sort_by(|a, b| a.id.cmp(&b.id));
    matrix.aggregates.sort_by(|a, b| {
        (
            &a.model_column_id,
            &a.capability,
            a.difficulty_tier,
            &a.dimension,
            a.as_of,
        )
            .cmp(&(
                &b.model_column_id,
                &b.capability,
                b.difficulty_tier,
                &b.dimension,
                b.as_of,
            ))
    });
    matrix
        .calibration
        .sort_by(|a, b| (&a.batch_id, &a.capability).cmp(&(&b.batch_id, &b.capability)));
    let mut aggregates = Vec::new();
    for row in &matrix.aggregates {
        serde_json::to_writer(&mut aggregates, row)?;
        aggregates.push(b'\n');
    }
    Ok(BTreeMap::from([
        (
            "catalog.json".to_owned(),
            serde_json::to_vec(&matrix.catalog)?,
        ),
        ("aggregates.jsonl".to_owned(), aggregates),
        (
            "calibration.json".to_owned(),
            serde_json::to_vec(&matrix.calibration)?,
        ),
        (
            "checks.json".to_owned(),
            serde_json::to_vec(&matrix.checks)?,
        ),
    ]))
}

/// Canonical JSON, sorted paths, zero uid/gid/mtime, and deterministic Ed25519 signatures.
pub fn build(
    mut manifest: Manifest,
    matrix: &PublicMatrix,
    keys: &SigningKeys,
) -> Result<Vec<u8>, BundleError> {
    if manifest.format != FORMAT || manifest.schema.min > manifest.schema.max {
        return Err(BundleError::Invalid("format or schema range"));
    }
    let mut files = payloads(matrix)?;
    manifest.files = files
        .iter()
        .map(|(path, bytes)| (path.clone(), sha256(bytes)))
        .collect();
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    for (kid, key) in &keys.0 {
        files.insert(
            format!("signatures/{kid}"),
            key.sign(&manifest_bytes).to_bytes().to_vec(),
        );
    }
    files.insert("manifest.json".to_owned(), manifest_bytes);
    let mut archive = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, format!("{FORMAT}/{path}"), bytes.as_slice())?;
    }
    Ok(archive.into_inner()?)
}

pub struct VerifiedBundle {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub matrix: PublicMatrix,
    pub signing_kids: Vec<String>,
}

fn read_files(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, BundleError> {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut files = BTreeMap::new();
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            return Err(BundleError::Invalid("non-regular archive entry"));
        }
        let path = entry.path_bytes();
        let path = std::str::from_utf8(&path).map_err(|_| BundleError::Invalid("non-UTF8 path"))?;
        let path = path
            .strip_prefix("mmbundle1/")
            .ok_or(BundleError::Invalid("archive root"))?
            .to_owned();
        if path != "manifest.json"
            && !PAYLOAD_FILES.contains(&path.as_str())
            && !path.strip_prefix("signatures/").is_some_and(valid_kid)
        {
            return Err(BundleError::Invalid("unexpected archive path"));
        }
        if files.contains_key(&path) {
            return Err(BundleError::Invalid("duplicate archive path"));
        }
        let declared = entry.size();
        if declared > bytes.len() as u64 {
            return Err(BundleError::Invalid("entry exceeds archive size"));
        }
        let mut data = Vec::new();
        (&mut entry)
            .take(bytes.len() as u64 + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 != declared {
            return Err(BundleError::Invalid("truncated entry"));
        }
        files.insert(path, data);
    }
    let consumed = archive.into_inner().position() as usize;
    if bytes[consumed..].iter().any(|b| *b != 0) {
        return Err(BundleError::Invalid("trailing archive data"));
    }
    Ok(files)
}

/// Parse and check all hashes. Origin publication uses this for its own retained files.
/// Replica import must call `verify`, which also authenticates the manifest.
pub fn inspect(bytes: &[u8]) -> Result<VerifiedBundle, BundleError> {
    let files = read_files(bytes)?;
    let manifest_bytes = files
        .get("manifest.json")
        .ok_or(BundleError::Invalid("missing manifest"))?
        .clone();
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    if !files.keys().any(|p| p.starts_with("signatures/")) {
        return Err(BundleError::Invalid("missing signatures"));
    }
    if manifest.format != FORMAT
        || manifest.schema.min > manifest.schema.max
        || manifest.files.len() != PAYLOAD_FILES.len()
    {
        return Err(BundleError::Invalid(
            "manifest format, schema range, or file set",
        ));
    }
    for path in PAYLOAD_FILES {
        let data = files
            .get(path)
            .ok_or(BundleError::Invalid("missing payload"))?;
        if manifest.files.get(path) != Some(&sha256(data)) {
            return Err(BundleError::Invalid("payload hash"));
        }
    }
    let aggregates = std::str::from_utf8(&files["aggregates.jsonl"])
        .map_err(|_| BundleError::Invalid("aggregate encoding"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()?;
    let matrix = PublicMatrix {
        catalog: serde_json::from_slice(&files["catalog.json"])?,
        aggregates,
        calibration: serde_json::from_slice(&files["calibration.json"])?,
        checks: serde_json::from_slice(&files["checks.json"])?,
    };
    Ok(VerifiedBundle {
        manifest,
        manifest_bytes,
        matrix,
        signing_kids: Vec::new(),
    })
}

/// At least one signature from the caller's compiled-in trust set must verify strictly.
pub fn verify(
    bytes: &[u8],
    trusted: &BTreeMap<String, VerifyingKey>,
    schema: i64,
) -> Result<VerifiedBundle, BundleError> {
    let files = read_files(bytes)?;
    let manifest_bytes = files
        .get("manifest.json")
        .ok_or(BundleError::Invalid("missing manifest"))?;
    let signing_kids: Vec<_> = trusted
        .iter()
        .filter_map(|(kid, key)| {
            let signature = Signature::from_slice(files.get(&format!("signatures/{kid}"))?).ok()?;
            key.verify_strict(manifest_bytes, &signature).ok()?;
            Some(kid.clone())
        })
        .collect();
    if signing_kids.is_empty() {
        return Err(BundleError::Invalid("no trusted signature"));
    }
    let mut bundle = inspect(bytes)?;
    if !(bundle.manifest.schema.min..=bundle.manifest.schema.max).contains(&schema) {
        return Err(BundleError::Invalid("schema outside supported range"));
    }
    bundle.signing_kids = signing_kids;
    Ok(bundle)
}

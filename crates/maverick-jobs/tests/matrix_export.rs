//! DIVERGENCE: D49. Public bundle privacy and export delivery on real Postgres.
use chrono::{TimeZone, Utc};
use ed25519_dalek::{
    SigningKey,
    pkcs8::{EncodePrivateKey, spki::der::pem::LineEnding},
};
use maverick_bundle::{IndexEntry, verify};
use maverick_jobs::matrix_export::{MatrixExportArgs, MatrixExportConfig, matrix_export};
use maverick_store::{MIGRATOR, corpus};
use maverick_testdb::Schema;
use serde_json::json;
use std::{collections::BTreeMap, fs, path::PathBuf, sync::LazyLock};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));
struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("mm-export-{}", ulid::Ulid::generate()));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn text_values(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|v| text_values(v, out)),
        serde_json::Value::Object(o) => o.values().for_each(|v| text_values(v, out)),
        _ => {}
    }
}

// Only authored answer/rubric text is secret here. Schema vocabulary such as
// "string" and "object" also occurs in the public check definitions.
fn answer_text(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(a) => a.iter().for_each(|v| answer_text(v, out)),
        serde_json::Value::Object(o) => {
            for (key, v) in o {
                if [
                    "value",
                    "equals",
                    "rubric",
                    "any_of",
                    "must_include",
                    "must_not_include",
                    "regex",
                ]
                .contains(&key.as_str())
                {
                    text_values(v, out);
                } else {
                    answer_text(v, out);
                }
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn public_export_excludes_corpus_and_populated_tenant_data_and_retries_immutably() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    // This is the checked-in synthetic parity corpus, never the private corpus.
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../parity/fixtures/measurement-io/testdata/internal/tasks/testdata/valid");
    let loaded = maverick_domain::tasks::load(&fixture).unwrap();
    corpus::store_corpus(
        &pool,
        "public-matrix",
        "private-description-canary",
        &loaded,
    )
    .await
    .unwrap();
    corpus::activate_corpus_version(&pool, "public-matrix")
        .await
        .unwrap();
    sqlx::raw_sql("insert into model_columns (id,provider,model_id,endpoint,status,pricing) values ('public-column','openai','public-model','direct','active','{\"input_per_mtok\":1}');
        insert into tenants (id,name,prompt_hmac_key) values ('tenant-id-canary','tenant-name-canary',decode('aabbcc','hex'));
        insert into api_keys (id,tenant_id,key_hash,last4,name) values ('key-id-canary','tenant-id-canary',convert_to('key-hash-canary','UTF8'),'1234','key-name-canary');
        insert into pools (id,tenant_id,name,model_column_ids) values ('pool-id-canary','tenant-id-canary','pool-name-canary',ARRAY['public-column']);
        insert into tenant_constraints (tenant_id,zero_retention) values ('tenant-id-canary',true);
        insert into tenant_weights values ('tenant-id-canary','coding',0.7,0.2,0.1,0.95);
        insert into recommendations (id,tenant_id,api_key_id,prompt_hash,request,classification,candidates,response,matrix_version) values ('rec-id-canary','tenant-id-canary','key-id-canary','prompt-hash-canary','{\"trace\":\"request-trace-canary\"}','{\"trace\":\"classification-canary\"}','{\"trace\":\"candidate-canary\"}','{\"trace\":\"response-canary\"}','public-matrix');
        insert into outcomes (id,recommendation_id,model_used,outcome,details) values ('out-id-canary','rec-id-canary','public-column','success','{\"detail\":\"outcome-canary\"}');
        insert into runs (id,batch_id,task_id,model_column_id,repeat_index,output,temperature,status,matrix_version) values ('run-id-canary','batch-private-canary','task_sx_017','public-column',0,'output-canary',0,'ok','public-matrix');
        insert into scores (run_id,grader_idx,dimension,value,detail) values ('run-id-canary',0,'quality',0.7,'{\"judge\":\"judge-text-canary\"}');
        insert into predictions values ('prediction-canary','public-column','coding',1,0.99,'tenant-blend-canary','public-matrix',now());
        insert into calibration values ('public-batch','coding',0.25,0.125,4,'2026-10-24T03:00:00Z');
        insert into aggregates (model_column_id,capability,difficulty_tier,dimension,ewma,variance,n,measured,matrix_version,as_of) values
          ('public-column','coding',1,'quality',0.75,0.125,4,true,'public-matrix','2026-09-25'),
          ('public-column','coding',1,'quality',0.8,0.1,5,true,'public-matrix','2026-10-24'),
          ('public-column','coding',1,'quality',0.2,0,1,false,'public-matrix','2026-09-24'),
          ('public-column','coding',1,'quality',0.3,0,1,false,'public-matrix','2026-10-25'),
          ('public-column','coding',2,'quality',0.4,0,1,false,'other-matrix','2026-10-24');").execute(&pool).await.unwrap();
    let dir = Dir::new();
    let key = SigningKey::from_bytes(&[23; 32]);
    let keypath = dir.0.join("runtime-key.pem");
    fs::write(
        &keypath,
        key.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let config = MatrixExportConfig {
        directory: dir.0.join("public"),
        signing_files: format!("test:{}", keypath.display()),
    };
    let args = MatrixExportArgs {
        night: "2026-10-24".into(),
        matrix_version: "public-matrix".into(),
        published_at: Utc.with_ymd_and_hms(2026, 10, 24, 3, 0, 0).unwrap(),
        rebuild_id: "rebuild-private-canary".into(),
    };
    let entry = matrix_export(&pool, &args, &config).await.unwrap();
    let bytes = fs::read(config.directory.join(&entry.file)).unwrap();
    let bundle = verify(
        &bytes,
        &BTreeMap::from([("test".into(), key.verifying_key())]),
        12,
    )
    .unwrap();
    assert_eq!(bundle.manifest.published_at, args.published_at);
    assert_eq!(bundle.manifest.matrix_version, args.matrix_version);
    assert_eq!(bundle.matrix.catalog.len(), 1);
    assert_eq!(bundle.matrix.aggregates.len(), 2);
    assert_eq!(bundle.matrix.aggregates[0].as_of.to_string(), "2026-09-25");
    assert_eq!(bundle.matrix.aggregates[1].ewma, 0.8);
    assert_eq!(bundle.matrix.calibration[0].brier_score, 0.25);
    assert!(!bundle.matrix.checks.is_empty());
    // Check the raw archive and decoded payloads: JSON escaping must not hide a leak.
    let raw = String::from_utf8_lossy(&bytes);
    let decoded = serde_json::to_string(&bundle.matrix).unwrap();
    let mut public_text = Vec::new();
    text_values(
        &serde_json::to_value(&bundle.matrix).unwrap(),
        &mut public_text,
    );
    for task in corpus::list_tasks_by_corpus_version(&pool, "public-matrix")
        .await
        .unwrap()
    {
        let graders: serde_json::Value = serde_json::from_str(task.grader_specs.get()).unwrap();
        let mut secrets = vec![
            task.id,
            task.prompt,
            serde_json::to_string(&graders).unwrap(),
        ];
        answer_text(&graders, &mut secrets);
        for secret in secrets {
            assert!(!raw.contains(&secret), "corpus field leaked in archive");
            assert!(
                !public_text.iter().any(|text| text.contains(&secret)),
                "corpus field leaked in decoded payload"
            );
        }
    }
    assert!(!raw.contains("canary"));
    assert!(!decoded.contains("canary"));
    // Add private golden/rubric canaries to a task and ensure neither can escape.
    sqlx::query("update tasks set grader_specs=$1 where id='task_sx_017'")
        .bind(json!([{"type":"golden","golden":"golden-answer-canary","rubric":"rubric-canary"}]))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update aggregates set ewma=0.1")
        .execute(&pool)
        .await
        .unwrap();
    let retry = matrix_export(&pool, &args, &config).await.unwrap();
    assert_eq!(retry, entry);
    assert_eq!(fs::read(config.directory.join(&entry.file)).unwrap(), bytes);
    let mut next = args.clone();
    next.published_at += chrono::Duration::days(1);
    let newer = matrix_export(&pool, &next, &config).await.unwrap();
    let nextbytes = fs::read(config.directory.join(&newer.file)).unwrap();
    assert!(!String::from_utf8_lossy(&nextbytes).contains("canary"));
    let index: Vec<IndexEntry> =
        serde_json::from_slice(&fs::read(config.directory.join("index.json")).unwrap()).unwrap();
    assert_eq!(index, vec![newer, entry]);
    pool.close().await;
    db.close().await;
}

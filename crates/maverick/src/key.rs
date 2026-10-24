//! `maverick key create | list | revoke`: the API keys of a single-tenant replica (spec
//! §14.4, D49). The tenant is implicit: the install's only tenant. Each command runs the
//! internal API's code (`maverick_api::provision`) and writes its audit line, with actor
//! `cli:<os user>`. The result is JSON on stdout. Logs and errors go to stderr.

use std::io::Write;

use maverick_api::provision;
use maverick_store::{is_not_found, tenancy};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::cli::{block_on, cli_actor, load_config};
use crate::logging::{Output, with_logger};

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// One `key` subcommand, after its flags parsed.
pub(crate) enum KeyCommand<'a> {
    Create { name: &'a str },
    List,
    Revoke { id: &'a str },
}

/// D49: an origin is multi-tenant, so it has no implicit tenant.
fn refused_on_origin(lookup: Lookup<'_>, stderr: &mut Output) -> bool {
    if lookup("MAVERICK_ROLE").as_deref() == Some("origin") {
        let _ = writeln!(stderr, "key commands need MAVERICK_ROLE=replica");
        true
    } else {
        false
    }
}

/// Runs `command`. Exit 0 with the JSON result on stdout, or 1 with the reason on stderr.
pub(crate) fn run(
    command: &KeyCommand<'_>,
    lookup: Lookup<'_>,
    stdout: Output,
    stderr: Output,
) -> i32 {
    let mut plain_err = stderr.clone();
    if refused_on_origin(lookup, &mut plain_err) {
        return EXIT_FAILURE;
    }
    let actor = cli_actor(lookup);
    let mut out = stdout;
    with_logger(stderr, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        let ran = block_on(async {
            let pool = match PgPoolOptions::new()
                .max_connections(2)
                .connect(&config.database_url)
                .await
            {
                Ok(pool) => pool,
                Err(error) => {
                    tracing::error!(error = %error, "connecting to database");
                    return Err(None);
                }
            };
            let result = execute(command, &pool, &actor).await;
            pool.close().await;
            result
        });
        match ran {
            Ok(Ok(json)) => {
                let _ = writeln!(out, "{json}");
                EXIT_SUCCESS
            }
            Ok(Err(Some(message))) => {
                let _ = writeln!(plain_err, "maverick key: {message}");
                EXIT_FAILURE
            }
            // A connect failure (already logged), or the runtime failed to build.
            Ok(Err(None)) | Err(_) => EXIT_FAILURE,
        }
    })
}

/// The command's JSON result, or the reason it failed (`None`: already logged).
async fn execute(
    command: &KeyCommand<'_>,
    pool: &PgPool,
    actor: &str,
) -> Result<String, Option<String>> {
    let failed = |error: sqlx::Error| Some(error.to_string());
    let tenant = the_tenant(pool).await?;
    match command {
        KeyCommand::Create { name } => {
            let key = provision::issue_key(pool, actor, &tenant.id, name)
                .await
                .map_err(failed)?;
            Ok(provision::issued_key_json(&key))
        }
        KeyCommand::List => {
            let keys = tenancy::list_api_keys_by_tenant(pool, &tenant.id)
                .await
                .map_err(failed)?;
            let rendered: Vec<String> = keys.iter().map(provision::api_key_json).collect();
            Ok(format!("[{}]", rendered.join(",")))
        }
        KeyCommand::Revoke { id } => {
            let missing = || Some(format!("api key {id} does not exist"));
            let key = match tenancy::get_api_key(pool, id).await {
                Ok(key) if key.tenant_id == tenant.id => key,
                Ok(_) => return Err(missing()),
                Err(error) if is_not_found(&error) => return Err(missing()),
                Err(error) => return Err(failed(error)),
            };
            let key = provision::revoke_key(pool, actor, key)
                .await
                .map_err(failed)?;
            Ok(provision::api_key_json(&key))
        }
    }
}

/// The install's only tenant. None yet means `serve` has not run its first start; more
/// than one means this is not a single-tenant install.
async fn the_tenant(pool: &PgPool) -> Result<tenancy::Tenant, Option<String>> {
    let mut tenants = tenancy::list_tenants(pool)
        .await
        .map_err(|error| Some(error.to_string()))?;
    match tenants.len() {
        1 => Ok(tenants.remove(0)),
        0 => Err(Some(
            "this install has no tenant yet; start maverick serve once to create it".to_owned(),
        )),
        count => Err(Some(format!(
            "this install has {count} tenants; key commands need a single-tenant replica"
        ))),
    }
}

//! The one Postgres server per test binary.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use testcontainers::bollard::Docker;
use testcontainers::bollard::models::{HostConfig, PortBinding};
use testcontainers::bollard::query_parameters::{ListContainersOptions, RemoveContainerOptions};
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tokio::sync::OnceCell;
use ulid::Ulid;

use crate::{DATABASE_URL_VAR, env_value};

/// The readiness line. Postgres prints it twice: once for the init server, once for the
/// real one.
const READY_LINE: &str = "database system is ready to accept connections";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

/// The only host address the container's port is published on. Docker-published ports
/// bypass host firewalls such as ufw, so publishing on every interface would put a
/// Postgres superuser on the network while tests run (model-maverick-l1q). A remote
/// Docker daemon is not supported; use `MAVERICK_TEST_DATABASE_URL` for one.
const PUBLISH_IP: &str = "127.0.0.1";
const POSTGRES_PORT: &str = "5432/tcp";

const LABEL: &str = "org.modelmaverick.testdb";
const LABEL_VERSION: &str = "1";
const PID_LABEL: &str = "org.modelmaverick.testdb.pid";
const HOST_LABEL: &str = "org.modelmaverick.testdb.host";
/// Where the owning process cannot be checked, a container older than this is an orphan.
const ORPHAN_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// A Postgres server that accepts `CREATE DATABASE`.
pub(crate) struct Server {
    /// URL of a database on the server that admin statements run in.
    pub(crate) admin_url: String,
    /// The Docker container that runs the server, when this crate started it.
    pub(crate) container_id: Option<String>,
}

pub(crate) enum Unavailable {
    /// No Docker daemon. The test is skipped (or fails under CI).
    Skip(String),
    /// Docker is there but the server could not start. The test fails.
    Fatal(String),
}

static SERVER: OnceCell<Result<Server, Unavailable>> = OnceCell::const_new();

/// Returns the server of this test binary. The first call starts it.
pub(crate) async fn server() -> Result<&'static Server, &'static Unavailable> {
    SERVER.get_or_init(start).await.as_ref()
}

async fn start() -> Result<Server, Unavailable> {
    if let Some(url) = env_value(DATABASE_URL_VAR) {
        if !url.contains("://") {
            return Err(Unavailable::Fatal(format!(
                "{DATABASE_URL_VAR} must be a postgres:// URL"
            )));
        }
        return Ok(Server {
            admin_url: url,
            container_id: None,
        });
    }

    let docker = Docker::connect_with_defaults()
        .map_err(|err| Unavailable::Skip(format!("Docker unavailable: {err}")))?;
    docker
        .ping()
        .await
        .map_err(|err| Unavailable::Skip(format!("Docker daemon unreachable: {err}")))?;
    reap_orphans(&docker).await;

    // A fresh password per server, so a leaked URL is useless once the binary exits.
    let password = format!("mv{}", Ulid::generate().to_string().to_lowercase());
    let container = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::log(
            LogWaitStrategy::stdout_or_stderr(READY_LINE).with_times(2),
        ))
        .with_env_var("POSTGRES_DB", "maverick")
        .with_env_var("POSTGRES_USER", "maverick")
        .with_env_var("POSTGRES_PASSWORD", password.as_str())
        // Durability settings only. Query, type, and transaction behavior is unchanged.
        .with_cmd([
            "postgres",
            "-c",
            "fsync=off",
            "-c",
            "synchronous_commit=off",
            "-c",
            "full_page_writes=off",
            "-c",
            "max_connections=300",
        ])
        .with_label(LABEL, LABEL_VERSION)
        .with_label(PID_LABEL, std::process::id().to_string())
        .with_label(HOST_LABEL, hostname())
        .with_startup_timeout(STARTUP_TIMEOUT)
        .with_host_config_modifier(publish_on_loopback)
        .start()
        .await
        .map_err(|err| Unavailable::Fatal(format!("start postgres:16-alpine: {err}")))?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .map_err(|err| Unavailable::Fatal(format!("postgres container port: {err}")))?;

    let container_id = container.id().to_owned();

    // The container must outlive every test in the binary, and Rust never drops statics.
    // The next test binary on this host removes it with `reap_orphans` once this process
    // has exited.
    std::mem::forget(container);

    Ok(Server {
        admin_url: format!(
            "postgres://maverick:{password}@{PUBLISH_IP}:{port}/maverick?sslmode=disable"
        ),
        container_id: Some(container_id),
    })
}

/// Publishes only the Postgres port, on [`PUBLISH_IP`], at a host port Docker picks. It
/// replaces the testcontainers default, which publishes every exposed port on all
/// interfaces.
fn publish_on_loopback(host_config: &mut HostConfig) {
    host_config.publish_all_ports = Some(false);
    host_config.port_bindings = Some(HashMap::from([(
        POSTGRES_PORT.to_owned(),
        Some(vec![PortBinding {
            host_ip: Some(PUBLISH_IP.to_owned()),
            host_port: Some(String::new()),
        }]),
    )]));
}

/// Removes containers that an earlier test binary on this host left behind. The Go
/// harness had Ryuk for this; testcontainers-rs has no reaper.
async fn reap_orphans(docker: &Docker) {
    let filters = HashMap::from([("label".to_owned(), vec![format!("{LABEL}={LABEL_VERSION}")])]);
    let options = ListContainersOptions {
        all: true,
        filters: Some(filters),
        ..Default::default()
    };
    let Ok(containers) = docker.list_containers(Some(options)).await else {
        return;
    };
    let host = hostname();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    for container in containers {
        let labels = container.labels.unwrap_or_default();
        if labels.get(HOST_LABEL) != Some(&host) {
            continue;
        }
        let pid = labels
            .get(PID_LABEL)
            .and_then(|pid| pid.parse::<u32>().ok());
        let created = container
            .created
            .and_then(|secs| u64::try_from(secs).ok())
            .map(Duration::from_secs);
        if !is_orphan(pid, created, now) {
            continue;
        }
        if let Some(id) = container.id {
            let options = RemoveContainerOptions {
                force: true,
                v: true,
                ..Default::default()
            };
            let _ = docker.remove_container(&id, Some(options)).await;
        }
    }
}

/// A container is an orphan when its owning process is gone. Where `/proc` is missing,
/// age decides.
fn is_orphan(pid: Option<u32>, created: Option<Duration>, now: Duration) -> bool {
    let Some(pid) = pid else {
        return true;
    };
    if pid == std::process::id() {
        return false;
    }
    let proc = Path::new("/proc");
    if proc.is_dir() {
        return !proc.join(pid.to_string()).exists();
    }
    created.is_some_and(|created| now.saturating_sub(created) > ORPHAN_AGE)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| env_value("HOSTNAME"))
        .or_else(|| env_value("COMPUTERNAME"))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use testcontainers::bollard::Docker;

    use super::{POSTGRES_PORT, PUBLISH_IP, is_orphan, server};
    use crate::{DATABASE_URL_VAR, Schema, env_value};

    /// model-maverick-l1q: Docker-published ports bypass ufw, so the server must be
    /// published on loopback only and never with the old fixed password.
    #[tokio::test]
    async fn the_server_is_published_on_loopback_only() {
        // `database` starts the server and applies the skip rule (a failure under CI).
        let Some(db) = crate::database(&Schema::empty()).await else {
            return;
        };
        db.close().await;
        if env_value(DATABASE_URL_VAR).is_some() {
            return;
        }
        let server = server().await.unwrap_or_else(|_| panic!("server started"));
        assert!(
            server.admin_url.starts_with("postgres://maverick:mv")
                && server.admin_url.contains(&format!("@{PUBLISH_IP}:")),
            "admin URL {}",
            server.admin_url
        );
        assert!(!server.admin_url.contains(":maverick@"));

        let id = server.container_id.as_deref().expect("the container");
        let docker = Docker::connect_with_defaults().expect("docker");
        let inspect = docker.inspect_container(id, None).await.expect("inspect");
        eprintln!("container {id}: {:?}", inspect.network_settings);
        let host_config = inspect.host_config.expect("host config");
        assert_eq!(host_config.publish_all_ports, Some(false));
        let requested = host_config.port_bindings.expect("port bindings");
        assert_eq!(requested.keys().collect::<Vec<_>>(), [POSTGRES_PORT]);
        let published = inspect
            .network_settings
            .and_then(|settings| settings.ports)
            .expect("published ports");
        let mut addresses = Vec::new();
        for bindings in published.values().flatten() {
            for binding in bindings {
                addresses.push(binding.host_ip.clone().unwrap_or_default());
            }
        }
        assert!(!addresses.is_empty(), "{published:?}");
        assert!(
            addresses.iter().all(|ip| ip == PUBLISH_IP),
            "published on {addresses:?}"
        );
    }

    #[test]
    fn a_container_without_an_owner_is_an_orphan() {
        assert!(is_orphan(None, None, Duration::ZERO));
    }

    #[test]
    fn a_container_of_this_process_is_not_an_orphan() {
        assert!(!is_orphan(Some(std::process::id()), None, Duration::ZERO));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_container_of_a_dead_process_is_an_orphan() {
        // Linux pid_max is at most 2^22, so this pid never exists.
        assert!(is_orphan(Some(u32::MAX), None, Duration::ZERO));
    }
}

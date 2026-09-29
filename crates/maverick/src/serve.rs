//! `maverick serve` after the config loaded (reference Part 1 §7.3): the startup order,
//! the listeners, the workers, and graceful shutdown under one 10-second deadline.
//!
//! On SIGINT or SIGTERM the servers stop accepting and drain, then the workers stop
//! taking jobs and let running handlers finish within what is left of the deadline (the
//! Rust design under D10/D11 replaces Go's hard cancel, QUIRK J9: an unfinished batch is
//! never marked completed and resumes under its lease). Then in-flight alert deliveries
//! drain.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use maverick_alerts::{AlertNotifier, Fanout};
use maverick_api::audit::JsonField;
use maverick_api::{
    ApiState, AuthState, RecommendConfig, internal_router_with, public_router_with,
};
use maverick_config::Config;
use maverick_domain::graders::judge_rubric::{
    FamilyResolver, JudgeClient, JudgeRequest, PoolColumn, pool_from_config,
};
use maverick_domain::graders::{
    BoxError, BoxFuture, GoldenAnswer, JsonSchema, JudgeRubric, KeyFacts, Registry, RuleScan,
};
use maverick_jobs::JobClient;
use maverick_jobs::worker::{self, RunningWorker, WorkerDeps};
use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::google::GoogleClient;
use maverick_providers::openai::OpenAiClient;
use maverick_providers::{CompletionRequest, ProviderClient};
use maverick_runner::Clients;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::logging::{self, Output};
use crate::otel::Otel;

/// The one deadline for every shutdown step.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);
/// Go's `ReadHeaderTimeout`.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Go's default maximum header size, 1 MiB, plus the 4 KiB slop Go's server allows.
const MAX_HEADER_BYTES: usize = (1 << 20) + 4096;

const EXIT_FAILURE: i32 = 1;

/// Runs `serve` with a loaded config and returns the exit code.
pub fn serve(config: Config, api_only: bool, worker_only: bool, stdout: Output) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return logging::with_logger(stdout, || {
                tracing::error!(error = %error, "starting the runtime");
                EXIT_FAILURE
            });
        }
    };
    runtime.block_on(async move {
        let otel = tracing::dispatcher::with_default(&logging::dispatch(stdout.clone()), || {
            Otel::init(&config)
        });
        let dispatch = logging::dispatch_with(stdout, otel.as_ref());
        // The global default reaches the runtime's worker threads; a process that already
        // set one (a test harness) keeps it, and this thread still logs through ours.
        let _ = tracing::dispatcher::set_global_default(dispatch.clone());
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let code = run(&config, api_only, worker_only).await;
        if let Some(otel) = &otel {
            otel.shutdown();
        }
        code
    })
}

/// Resolves on SIGINT or SIGTERM. The handlers are installed when this is created, so a
/// signal during startup is not lost.
struct ShutdownSignal {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl ShutdownSignal {
    fn install() -> Self {
        ShutdownSignal {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .ok(),
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        let terminate = async {
            match self.terminate.as_mut() {
                Some(signal) => {
                    signal.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminate => {}
        }
    }
}

async fn run(config: &Config, api_only: bool, worker_only: bool) -> i32 {
    tracing::info!(
        public_addr = config.public_addr.as_str(),
        internal_addr = config.internal_addr.as_str(),
        api_only,
        worker_only,
        "starting maverick"
    );
    let mut signal = ShutdownSignal::install();

    // Connections are lazy: an unreachable database does not fail startup.
    let max_connections = std::thread::available_parallelism()
        .map_or(4, |n| u32::try_from(n.get()).unwrap_or(4))
        .max(4);
    let pool = match PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_lazy(&config.database_url)
    {
        Ok(pool) => pool,
        Err(error) => {
            tracing::error!(error = %error, "creating database pool");
            return EXIT_FAILURE;
        }
    };

    let providers = provider_clients(config);
    let fanout = match Fanout::new(
        &config.smtp_url,
        &config.alerts_webhook_url,
        &config.alerts_webhook_secret,
    ) {
        Ok(fanout) => Arc::new(fanout),
        Err(error) => {
            tracing::error!(error = %error, "configuring alert delivery");
            return EXIT_FAILURE;
        }
    };
    let notifier: Arc<dyn AlertNotifier> = fanout.clone();

    let mut workers: Option<RunningWorker> = None;
    if !api_only {
        let graders = grader_registry(config, &pool, &providers).await;
        let client = match JobClient::from_config(config) {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(error = %error, "creating job client");
                return EXIT_FAILURE;
            }
        };
        let deps = WorkerDeps {
            clients: providers
                .iter()
                .fold(Clients::new(), |clients, (name, client)| {
                    clients.with(name, Arc::clone(client))
                }),
            graders: Arc::new(graders),
            notifier: Some(Arc::clone(&notifier)),
            price_source: None,
        };
        let options = worker::worker_options(&client, config, deps)
            .pg_pool(pool.clone())
            .shutdown_grace_period(SHUTDOWN_DEADLINE);
        match worker::start(options).await {
            Ok(running) => workers = Some(running),
            Err(error) => {
                tracing::error!(error = %error, "starting workers");
                return EXIT_FAILURE;
            }
        }
        tracing::info!("workers started");
    }

    let mut recommend = RecommendConfig::from_config(config);
    recommend.alerts = Some(Arc::clone(&notifier));
    // Step 10, the classifier resolver (unless --worker-only), hooks in here with the
    // prompt path (bead 5.6): list the catalog, connect the classifier columns' clients,
    // and on failure log `classifier column unavailable; prompt-path requests will
    // receive the fallback classification` and continue.
    let auth = match AuthState::from_config(pool.clone(), config) {
        Ok(auth) => auth,
        Err(error) => {
            tracing::error!(error = %error, "configuring authentication");
            return EXIT_FAILURE;
        }
    };
    let api = ApiState::new(pool.clone(), config).with_recommend_config(recommend);

    let mut code = 0;
    let mut servers = Vec::new();
    let mut listeners = vec![(
        "internal",
        config.internal_addr.as_str(),
        internal_router_with(auth.clone(), api.clone()),
    )];
    if !worker_only {
        listeners.push((
            "public",
            config.public_addr.as_str(),
            public_router_with(auth, api),
        ));
    }
    for (name, addr, router) in listeners {
        tracing::info!(listener = name, addr, "listening");
        match TcpListener::bind(listen_addr(addr)).await {
            Ok(listener) => servers.push(Server::spawn(name, listener, router)),
            Err(error) => {
                tracing::error!(listener = name, error = %error, "listener failed");
                code = EXIT_FAILURE;
                break;
            }
        }
    }

    if code == 0 {
        signal.recv().await;
        tracing::info!("shutdown signal received");
    }

    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    for server in servers {
        let name = server.name;
        if let Err(error) = server.shutdown(deadline).await {
            tracing::error!(listener = name, error = %error, "shutdown failed");
            code = EXIT_FAILURE;
        }
    }
    if let Some(running) = workers {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Err(error) = running.stop(left).await {
            tracing::error!(error = %error, "stopping workers");
            code = EXIT_FAILURE;
        }
    }
    fanout.wait().await;
    pool.close().await;
    tracing::info!("stopped");
    code
}

/// Go's `":8080"` (every interface) as a bindable address.
fn listen_addr(addr: &str) -> String {
    if addr.starts_with(':') {
        format!("0.0.0.0{addr}")
    } else {
        addr.to_owned()
    }
}

/// A built provider client, or why it could not be built.
type Built = Result<Arc<dyn ProviderClient>, String>;
/// A provider's name, its API key, and how to build its client.
type Candidate<'a> = (&'static str, &'a str, fn(&str) -> Built);

fn shared<C: ProviderClient + 'static>(built: Result<C, impl std::fmt::Display>) -> Built {
    built
        .map(|client| Arc::new(client) as Arc<dyn ProviderClient>)
        .map_err(|error| error.to_string())
}

/// One client per provider whose API key is set, in the order anthropic, openai, xai,
/// google. A client that cannot be built is logged and left out.
fn provider_clients(config: &Config) -> Vec<(&'static str, Arc<dyn ProviderClient>)> {
    let candidates: [Candidate<'_>; 4] = [
        ("anthropic", &config.anthropic_api_key, |key| {
            shared(AnthropicClient::new(key))
        }),
        ("openai", &config.openai_api_key, |key| {
            shared(OpenAiClient::openai(key))
        }),
        ("xai", &config.xai_api_key, |key| {
            shared(OpenAiClient::xai(key))
        }),
        ("google", &config.google_api_key, |key| {
            shared(GoogleClient::new(key))
        }),
    ];
    candidates
        .into_iter()
        .filter(|(_, key, _)| !key.is_empty())
        .filter_map(|(name, key, build)| match build(key) {
            Ok(client) => Some((name, client)),
            Err(error) => {
                tracing::error!(error = %error, "configuring {name} provider client");
                None
            }
        })
        .collect()
}

/// A provider client as a judge: a plain text completion with no settings.
struct ProviderJudge(Arc<dyn ProviderClient>);

impl JudgeClient for ProviderJudge {
    fn complete<'a>(&'a self, request: JudgeRequest) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            let response = self
                .0
                .complete(CompletionRequest {
                    model: request.model,
                    system: request.system,
                    prompt: request.prompt,
                    temperature: request.temperature,
                    max_tokens: request.max_tokens,
                    settings: serde_json::Map::new(),
                })
                .await?;
            Ok(response.text)
        })
    }
}

/// The judged column's family: its provider in the catalog.
struct StoreFamilies(PgPool);

impl FamilyResolver for StoreFamilies {
    fn family<'a>(&'a self, column_id: &'a str) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            let column = maverick_store::catalog::get_model_column(&self.0, column_id).await?;
            Ok(column.provider)
        })
    }
}

/// json_schema, rule_scan, golden_answer, and key_facts always; judge_rubric when the
/// judge pool resolves against the catalog and the provider clients. Otherwise judge
/// gradings fail (a validation-only instance) and serving continues.
async fn grader_registry(
    config: &Config,
    pool: &PgPool,
    providers: &[(&'static str, Arc<dyn ProviderClient>)],
) -> Registry {
    let mut registry = Registry::new();
    registry.must_register(Arc::new(JsonSchema));
    registry.must_register(Arc::new(RuleScan));
    registry.must_register(Arc::new(GoldenAnswer));
    registry.must_register(Arc::new(KeyFacts));
    let judges: HashMap<String, Arc<dyn JudgeClient>> = providers
        .iter()
        .map(|(name, client)| {
            (
                (*name).to_owned(),
                Arc::new(ProviderJudge(Arc::clone(client))) as Arc<dyn JudgeClient>,
            )
        })
        .collect();
    let judge = async {
        let columns = maverick_store::catalog::list_model_columns(pool)
            .await
            .map_err(|error| error.to_string())?;
        let catalog: Vec<PoolColumn<'_>> = columns
            .iter()
            .map(|column| PoolColumn {
                model_id: &column.model_id,
                endpoint: &column.endpoint,
                provider: &column.provider,
            })
            .collect();
        let refs: Vec<&str> = config.judge_pool.iter().map(String::as_str).collect();
        let members = pool_from_config(&refs, &catalog, &judges)?;
        JudgeRubric::new(members, Some(Arc::new(StoreFamilies(pool.clone()))))
    }
    .await;
    match judge {
        Ok(judge) => registry.must_register(Arc::new(judge)),
        Err(error) => {
            tracing::error!(
                pool = ?JsonField::of(&config.judge_pool),
                error = %error,
                "judge pool unavailable; judge_rubric gradings will fail until it resolves"
            );
            registry.must_register(Arc::new(JudgeRubric::validator()));
        }
    }
    registry
}

/// One HTTP listener: the accept loop runs until [`Server::shutdown`].
struct Server {
    name: &'static str,
    stop: oneshot::Sender<()>,
    accepting: JoinHandle<GracefulShutdown>,
}

impl Server {
    fn spawn(name: &'static str, listener: TcpListener, app: Router) -> Server {
        let (stop, stopped) = oneshot::channel();
        let accepting = tokio::spawn(accept(listener, app, stopped));
        Server {
            name,
            stop,
            accepting,
        }
    }

    /// Stops accepting and waits for in-flight requests until `deadline`.
    async fn shutdown(self, deadline: Instant) -> Result<(), String> {
        let _ = self.stop.send(());
        let graceful = self.accepting.await.map_err(|e| e.to_string())?;
        tokio::time::timeout_at(deadline, graceful.shutdown())
            .await
            .map_err(|_| "in-flight requests did not finish before the deadline".to_owned())
    }
}

/// HTTP/1.1 with Go's 10-second header read timeout; every connection is watched for the
/// graceful drain.
async fn accept(
    listener: TcpListener,
    app: Router,
    mut stopped: oneshot::Receiver<()>,
) -> GracefulShutdown {
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .max_buf_size(MAX_HEADER_BYTES);
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::warn!(error = %error, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let service = TowerToHyperService::new(app.clone());
                let connection =
                    graceful.watch(http.serve_connection(TokioIo::new(stream), service));
                tokio::spawn(async move {
                    let _ = connection.await;
                });
            }
            _ = &mut stopped => break,
        }
    }
    graceful
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_style_addresses_bind_every_interface() {
        assert_eq!(listen_addr(":8080"), "0.0.0.0:8080");
        assert_eq!(listen_addr("127.0.0.1:8081"), "127.0.0.1:8081");
    }
}

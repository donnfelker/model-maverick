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
use maverick_api::classify::{Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, PgAuthBackend, PublicAuthBackend, PublicAuthDecision, RecommendConfig,
    internal_router_with, mcp, public_router_with,
};
use maverick_config::Config;
use maverick_domain::graders::judge_rubric::{FamilyResolver, JudgeClient, JudgeRequest};
use maverick_domain::graders::{
    BoxError, BoxFuture, GoldenAnswer, JsonSchema, KeyFacts, Registry, RuleScan,
};
use maverick_jobs::JobClient;
use maverick_jobs::worker::{self, RunningWorker, WorkerDeps};
use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::google::GoogleClient;
use maverick_providers::models::ModelLister;
use maverick_providers::openai::OpenAiClient;
use maverick_providers::{CompletionRequest, ProviderClient};
use maverick_runner::Clients;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::logging::{self, Output};
use crate::otel::Otel;
use crate::rewatch::{self, JudgeSlot, Rewatch};

/// `mcp-stdio --api-key <key>` after the flags and the config (reference Part 1 §7.4).
/// The key is authenticated once, and the whole session acts as it. stdout is the MCP
/// channel and carries nothing else; logs go to stderr.
pub fn mcp_stdio(config: Config, api_key: &str, stderr: Output) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return logging::with_logger(stderr, || {
                tracing::error!(error = %error, "starting the runtime");
                EXIT_FAILURE
            });
        }
    };
    runtime.block_on(async move {
        let dispatch = logging::dispatch(stderr);
        let _ = tracing::dispatcher::set_global_default(dispatch.clone());
        let _guard = tracing::dispatcher::set_default(&dispatch);
        stdio_session(&config, api_key).await
    })
}

async fn stdio_session(config: &Config, api_key: &str) -> i32 {
    let pool = match pool_options(4).connect_lazy(&config.database_url) {
        Ok(pool) => pool,
        Err(error) => {
            tracing::error!(error = %error, "creating database pool");
            return EXIT_FAILURE;
        }
    };
    // One direct connection first: an unreachable database fails at once, as Go's did,
    // instead of after the pool's connect retries.
    if let Err(error) = PgConnection::connect(&config.database_url).await {
        tracing::error!(
            error = %format!("looking up api key: {error}"),
            "mcp-stdio: authenticating --api-key"
        );
        return EXIT_FAILURE;
    }
    let identity = match PgAuthBackend::new(pool.clone()).authenticate(api_key).await {
        Ok(PublicAuthDecision::Authenticated(identity)) => identity,
        Ok(PublicAuthDecision::Missing) => {
            tracing::error!(
                error = "missing or invalid API key",
                "mcp-stdio: authenticating --api-key"
            );
            return EXIT_FAILURE;
        }
        Ok(PublicAuthDecision::Suspended) => {
            tracing::error!(
                error = "tenant is suspended",
                "mcp-stdio: authenticating --api-key"
            );
            return EXIT_FAILURE;
        }
        Err(error) => {
            let error = match &error {
                maverick_api::AuthBackendError::Store(source) => {
                    format!("looking up api key: {source}")
                }
            };
            tracing::error!(error = %error, "mcp-stdio: authenticating --api-key");
            return EXIT_FAILURE;
        }
    };
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
    let providers = provider_clients(config);
    let mut recommend = RecommendConfig::from_config(config);
    recommend.alerts = Some(Arc::clone(&notifier));
    recommend.classifier =
        Arc::new(classifier_resolver(config, &pool, &providers, Arc::clone(&notifier)).await);
    let api = ApiState::new(pool.clone(), config).with_recommend_config(recommend);

    tracing::info!(
        tenant_id = identity.tenant_id.as_str(),
        "mcp-stdio session starting"
    );
    let session = mcp::serve_lines(
        &api,
        &identity,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await;
    fanout.wait().await;
    pool.close().await;
    match session {
        Ok(()) => {
            tracing::info!("mcp-stdio session ended");
            0
        }
        Err(error) => {
            tracing::error!(error = %error, "mcp-stdio session failed");
            EXIT_FAILURE
        }
    }
}

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
    let pool = match pool_options(max_connections).connect_lazy(&config.database_url) {
        Ok(pool) => pool,
        Err(error) => {
            tracing::error!(error = %error, "creating database pool");
            return EXIT_FAILURE;
        }
    };

    let built = built_clients(config);
    let model_listers: Vec<Arc<dyn ModelLister>> = built
        .iter()
        .map(|(_, client)| Arc::clone(&client.listing))
        .collect();
    let providers: Vec<(&'static str, Arc<dyn ProviderClient>)> = built
        .into_iter()
        .map(|(name, client)| (name, client.completions))
        .collect();
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
    // D41: background re-resolution of the judge pool and the classifier columns.
    let mut watchers: Vec<JoinHandle<()>> = Vec::new();
    if !api_only {
        let graders = grader_registry(config, &pool, &providers).await;
        watchers.push(tokio::spawn(rewatch::watch_judge(
            Arc::clone(&graders.judge),
            config.judge_pool.clone(),
            pool.clone(),
            graders.judges,
            graders.families,
            Rewatch::DEFAULT,
        )));
        let graders = graders.registry;
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
            model_listers: model_listers.clone(),
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
    // Step 10: the classifier resolver (unless --worker-only). A column that does not
    // resolve logs `classifier column unavailable; ...` and serving continues.
    if !worker_only {
        let resolver =
            Arc::new(classifier_resolver(config, &pool, &providers, Arc::clone(&notifier)).await);
        watchers.push(tokio::spawn(rewatch::watch_classifier(
            Arc::clone(&resolver),
            pool.clone(),
            client_map(&providers),
            Arc::clone(&notifier),
            Rewatch::DEFAULT,
        )));
        recommend.classifier = resolver;
    }
    let auth = match AuthState::from_config(pool.clone(), config) {
        Ok(auth) => auth,
        Err(error) => {
            tracing::error!(error = %error, "configuring authentication");
            return EXIT_FAILURE;
        }
    };
    let api = ApiState::new(pool.clone(), config)
        .with_recommend_config(recommend)
        .with_model_listers(model_listers);

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
    for watcher in watchers {
        watcher.abort();
    }
    fanout.wait().await;
    pool.close().await;
    tracing::info!("stopped");
    code
}

/// A pooled connection idle at least this long is pinged before use (e3x).
///
/// Not Go's 1 s (pgxpool's `ShouldPing`). pgxpool's idle list is a stack, so it hands back
/// the connection used last. sqlx's idle queue is FIFO: every acquire takes the connection
/// idle longest. With a 1 s threshold, light traffic would ping on nearly every acquire.
/// 30 s keeps the ping off the request path at any traffic where latency counts. sqlx's
/// own `before_acquire` example uses 60 s.
const PING_IDLE: Duration = Duration::from_secs(30);

/// The engine's database pool. sqlx's default `test_before_acquire` pings the connection on
/// every acquire: one more round trip in front of every query, and most of the declared
/// recommend path's cost (e3x). sqlx also pings each connection when it goes back to the
/// pool, off the request path, and closes it if that ping fails. So only a connection
/// idle for [`PING_IDLE`] is pinged here. A failed ping closes it, and the acquire opens
/// another. A connection that dies while idle for less (a Postgres restart) fails its
/// next query, and sqlx closes it on release.
fn pool_options(max_connections: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .test_before_acquire(false)
        .before_acquire(|conn, meta| {
            Box::pin(async move {
                if meta.idle_for >= PING_IDLE {
                    conn.ping().await?;
                }
                Ok(true)
            })
        })
}

/// Go's `":8080"` (every interface) as a bindable address.
fn listen_addr(addr: &str) -> String {
    if addr.starts_with(':') {
        format!("0.0.0.0{addr}")
    } else {
        addr.to_owned()
    }
}

/// Step 10: the prompt path's resolver, its classifier columns connected to the provider
/// clients through the catalog. Without the catalog, every prompt request gets the
/// fallback classification.
async fn classifier_resolver(
    config: &Config,
    pool: &PgPool,
    providers: &[(&'static str, Arc<dyn ProviderClient>)],
    notifier: Arc<dyn AlertNotifier>,
) -> Resolver {
    let resolver = Resolver::new(ResolverSettings::from_config(config)).with_alerts(notifier);
    match maverick_store::catalog::list_model_columns(pool).await {
        Ok(columns) => resolver.connect(&columns, &client_map(providers)),
        Err(error) => {
            tracing::error!(
                column = %config.classifier_column,
                error = %error,
                "classifier column unavailable; prompt-path requests will receive the \
                 fallback classification"
            );
            resolver
        }
    }
}

/// The provider clients by provider name.
fn client_map(
    providers: &[(&'static str, Arc<dyn ProviderClient>)],
) -> HashMap<String, Arc<dyn ProviderClient>> {
    providers
        .iter()
        .map(|(name, client)| ((*name).to_owned(), Arc::clone(client)))
        .collect()
}

/// A provider client built from the operator config: the same client for completions
/// and for its model list (spec §4.6).
struct BuiltClient {
    completions: Arc<dyn ProviderClient>,
    listing: Arc<dyn ModelLister>,
}

/// A built provider client, or why it could not be built.
type Built = Result<BuiltClient, String>;
/// A provider's name, its API key, its base URL (empty is the default), and how to build
/// its client.
type Candidate<'a> = (&'static str, &'a str, &'a str, fn(&str, &str) -> Built);

fn shared<C: ProviderClient + ModelLister + 'static>(
    built: Result<C, impl std::fmt::Display>,
) -> Built {
    built
        .map(|client| {
            let client = Arc::new(client);
            BuiltClient {
                completions: Arc::clone(&client) as Arc<dyn ProviderClient>,
                listing: client,
            }
        })
        .map_err(|error| error.to_string())
}

/// The completion clients of [`built_clients`].
fn provider_clients(config: &Config) -> Vec<(&'static str, Arc<dyn ProviderClient>)> {
    built_clients(config)
        .into_iter()
        .map(|(name, client)| (name, client.completions))
        .collect()
}

/// One client per provider whose API key is set, in the order anthropic, openai, xai,
/// google. A non-empty `MAVERICK_<PROVIDER>_BASE_URL` replaces the provider's default URL.
/// A client that cannot be built is logged and left out, so `model_discovery` reports
/// its provider as `skipped_no_key`.
fn built_clients(config: &Config) -> Vec<(&'static str, BuiltClient)> {
    let candidates: [Candidate<'_>; 4] = [
        (
            "anthropic",
            &config.anthropic_api_key,
            &config.anthropic_base_url,
            |key, url| {
                shared(AnthropicClient::new(key).map(|client| match url {
                    "" => client,
                    url => client.with_base_url(url),
                }))
            },
        ),
        (
            "openai",
            &config.openai_api_key,
            &config.openai_base_url,
            |key, url| {
                shared(OpenAiClient::openai(key).map(|client| match url {
                    "" => client,
                    url => client.with_base_url(url),
                }))
            },
        ),
        (
            "xai",
            &config.xai_api_key,
            &config.xai_base_url,
            |key, url| {
                shared(OpenAiClient::xai(key).map(|client| match url {
                    "" => client,
                    url => client.with_base_url(url),
                }))
            },
        ),
        (
            "google",
            &config.google_api_key,
            &config.google_base_url,
            |key, url| {
                shared(GoogleClient::new(key).map(|client| match url {
                    "" => client,
                    url => client.with_base_url(url),
                }))
            },
        ),
    ];
    candidates
        .into_iter()
        .filter(|(_, key, _, _)| !key.is_empty())
        .filter_map(|(name, key, url, build)| match build(key, url) {
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

/// The worker's graders, and what the judge watcher needs.
struct Graders {
    registry: Registry,
    judge: Arc<JudgeSlot>,
    judges: HashMap<String, Arc<dyn JudgeClient>>,
    families: Arc<dyn FamilyResolver>,
}

/// json_schema, rule_scan, golden_answer, and key_facts always; judge_rubric through a
/// [`JudgeSlot`] that grades once the judge pool resolves against the catalog and the
/// provider clients. Until then judge gradings fail (the validation-only instance),
/// serving continues, and [`rewatch::watch_judge`] keeps trying (D41).
async fn grader_registry(
    config: &Config,
    pool: &PgPool,
    providers: &[(&'static str, Arc<dyn ProviderClient>)],
) -> Graders {
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
    let families: Arc<dyn FamilyResolver> = Arc::new(StoreFamilies(pool.clone()));
    let judge =
        rewatch::resolve_judge(&config.judge_pool, pool, &judges, Arc::clone(&families)).await;
    let judge = match judge {
        Ok(judge) => Arc::new(JudgeSlot::new(Some(judge))),
        Err(error) => {
            tracing::error!(
                pool = ?JsonField::of(&config.judge_pool),
                error = %error,
                "judge pool unavailable; judge_rubric gradings will fail until it resolves"
            );
            Arc::new(JudgeSlot::new(None))
        }
    };
    registry.must_register(Arc::clone(&judge) as Arc<dyn maverick_domain::graders::Grader>);
    Graders {
        registry,
        judge,
        judges,
        families,
    }
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

    /// `MAVERICK_<PROVIDER>_BASE_URL` moves that provider's client (D23: only the engine's
    /// own config does), and a provider without one keeps its default URL.
    #[tokio::test]
    async fn a_provider_base_url_redirects_its_client() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let app = axum::Router::new().fallback({
            let seen = Arc::clone(&seen);
            move |request: axum::extract::Request| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().unwrap().push(request.uri().path().to_owned());
                    axum::Json(serde_json::json!({
                        "content": [{"type": "text", "text": "ok"}],
                        "usage": {"input_tokens": 1, "output_tokens": 1},
                    }))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config = Config::load_with(|name| match name {
            "MAVERICK_DATABASE_URL" => Some("postgres://db".to_owned()),
            "MAVERICK_INTERNAL_TOKENS" => Some("ops:alpha".to_owned()),
            "MAVERICK_CAPTURE_MASTER_KEY" => Some("ab".repeat(32)),
            "MAVERICK_ANTHROPIC_API_KEY" => Some("test-key".to_owned()),
            "MAVERICK_ANTHROPIC_BASE_URL" => Some(url.clone()),
            _ => None,
        })
        .unwrap();
        let clients = provider_clients(&config);
        let names: Vec<&str> = clients.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["anthropic"], "only providers with a key");
        let _ = clients[0]
            .1
            .complete(CompletionRequest {
                model: "claude-test".to_owned(),
                system: String::new(),
                prompt: "hi".to_owned(),
                temperature: 0.0,
                max_tokens: 8,
                settings: serde_json::Map::new(),
            })
            .await;
        assert_eq!(*seen.lock().unwrap(), ["/v1/messages"]);

        // Spec §4.6: the same configured client lists the provider's models, on the same
        // base URL, so `model_discovery` never reaches a provider the config did not name.
        let built = built_clients(&config);
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].1.listing.provider(), "anthropic");
        built[0].1.listing.list_models().await.expect("list");
        assert_eq!(*seen.lock().unwrap(), ["/v1/messages", "/v1/models"]);
    }
}

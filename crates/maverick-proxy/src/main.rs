//! The trial proxy binary.
//!
//! Reads its configuration from the environment, logs JSON lines to stdout, serves until
//! SIGINT or SIGTERM, drains for 10 s, and exits 1 on a configuration, listener, or
//! shutdown error (reference part 6 §2).

use std::process::ExitCode;
use std::sync::Arc;

use maverick_proxy::config::{Config, listen_addr, load_config};
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::server::{Proxy, ServeSettings, serve, shutdown_signal};
use maverick_proxy::surface::ExecutorRunner;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(std::io::stdout)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = match load_config(|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error, "maverick-proxy: invalid configuration");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "maverick-proxy: cannot start the runtime");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(config))
}

async fn run(config: Config) -> ExitCode {
    let addr = listen_addr(&config.addr);
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(error = %error, addr = %addr, "maverick-proxy: listen failed");
            return ExitCode::FAILURE;
        }
    };
    let mode = config.mode.as_str();
    let reports = match ProxyReports::from_config(&config) {
        Ok(reports) => Arc::new(reports),
        Err(error) => {
            tracing::error!(error = %error, "maverick-proxy: cannot build the telemetry client");
            return ExitCode::FAILURE;
        }
    };
    let runner = match ExecutorRunner::new(&config, reports.clone()) {
        Ok(runner) => runner,
        Err(error) => {
            tracing::error!(error = %error, "maverick-proxy: cannot build the upstream client");
            return ExitCode::FAILURE;
        }
    };
    let proxy = match Proxy::new(config, Arc::new(runner)) {
        Ok(proxy) => proxy,
        Err(error) => {
            tracing::error!(error = %error, "maverick-proxy: cannot build the engine client");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(addr = %addr, mode, "maverick-proxy: listening");
    let served = serve(
        listener,
        proxy.router(),
        ServeSettings::default(),
        shutdown_signal(),
    )
    .await;
    // D27: flush queued telemetry, within the batcher's bound, before exiting.
    reports.shutdown().await;
    match served {
        Ok(()) => {
            tracing::info!("maverick-proxy: stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(error = %error, "maverick-proxy: shutdown failed");
            ExitCode::FAILURE
        }
    }
}

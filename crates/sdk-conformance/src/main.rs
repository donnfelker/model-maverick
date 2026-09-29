//! Runner for the SDK conformance harness (sdk-v0 plan §2.3).
//!
//! `sdk-conformance serve` starts the mock engine and the mock provider, prints their
//! addresses as one JSON line, and serves until it is interrupted. An SDK repo in any
//! language starts it, reads the ports, drives scenarios through the control API, and reads
//! the recorded requests back.

use std::io::Write as _;
use std::process::ExitCode;

use sdk_conformance::{Harness, Scenario};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sdk-conformance: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Vec<String>) -> Result<(), String> {
    let mut iter = args.iter();
    match iter.next().map(String::as_str) {
        Some("serve") => {}
        None | Some("--help" | "-h") => {
            print_usage();
            return Ok(());
        }
        Some(other) => {
            return Err(format!(
                "unknown command {other:?}; try `serve` or `--help`"
            ));
        }
    }

    let mut engine_port = 0u16;
    let mut provider_port = 0u16;
    let mut scenario_file: Option<String> = None;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--engine-port" => engine_port = parse_port(iter.next())?,
            "--provider-port" => provider_port = parse_port(iter.next())?,
            "--scenario" => {
                scenario_file = Some(iter.next().ok_or("--scenario needs a path")?.clone());
            }
            other => return Err(format!("unknown flag {other:?}; try `--help`")),
        }
    }

    let harness = Harness::start_on(engine_port, provider_port)
        .await
        .map_err(|error| error.to_string())?;

    if let Some(path) = scenario_file {
        let bytes = std::fs::read(&path).map_err(|error| format!("read {path}: {error}"))?;
        let scenario =
            Scenario::from_json(&bytes).map_err(|error| format!("parse {path}: {error}"))?;
        harness.load(scenario);
    }

    // One JSON line, so a CI script in any language can read the two addresses.
    println!(
        "{{\"engine\":\"{}\",\"provider\":\"{}\"}}",
        harness.engine_base_url(),
        harness.provider_base_url()
    );
    let _ = std::io::stdout().flush();

    tokio::signal::ctrl_c()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn parse_port(value: Option<&String>) -> Result<u16, String> {
    value
        .ok_or("a port flag needs a number")?
        .parse::<u16>()
        .map_err(|error| format!("bad port: {error}"))
}

fn print_usage() {
    println!(
        "sdk-conformance serve [--engine-port N] [--provider-port N] [--scenario FILE]\n\n\
         Starts the mock MM engine and the mock provider on 127.0.0.1 (ephemeral ports by\n\
         default) and prints their addresses as one JSON line. Load scenarios and read\n\
         recorded requests through the engine server's /control endpoints."
    );
}

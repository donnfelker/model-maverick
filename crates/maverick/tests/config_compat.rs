//! Configuration compatibility through the binary's entry point (spec §14.5
//! "Configuration", D50): an unknown `MAVERICK_*` variable gets one warning with its name
//! and never its value, startup goes on, and the settings required today are enough.

use std::io::{self, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use maverick_cli::{Output, run, run_with_env};
use serde_json::Value;

#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The settings every engine needs today, with a database nothing listens on.
fn required() -> Vec<(String, String)> {
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("address").port();
    drop(closed);
    [
        ("MAVERICK_ROLE", "origin".to_owned()),
        (
            "MAVERICK_DATABASE_URL",
            format!("postgres://maverick:maverick@127.0.0.1:{port}/maverick"),
        ),
        ("MAVERICK_INTERNAL_TOKENS", "ops:compat-secret".to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32)),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

/// Runs `migrate` (it loads the configuration, then fails on the closed database) and
/// returns the JSON log lines on stdout.
fn migrate(env: &[(String, String)], with_names: bool) -> Vec<Value> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let args = vec!["migrate".to_owned()];
    let stdout = Output::new(Shared(out.clone()));
    let stderr = Output::new(io::sink());
    let code = if with_names {
        let names = env.iter().map(|(name, _)| name.clone()).collect();
        run_with_env(&args, &lookup, names, stdout, stderr)
    } else {
        run(&args, &lookup, stdout, stderr)
    };
    assert_eq!(code, 1, "migrate fails on the closed database");
    let text = String::from_utf8(out.lock().expect("lock").clone()).expect("utf8");
    assert!(!text.contains("super-secret-value"), "{text}");
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn warnings(lines: &[Value]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter(|line| line["level"] == "WARN")
        .map(|line| {
            (
                line["msg"].as_str().unwrap_or_default().to_owned(),
                line["var"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// DIVERGENCE D50: an unknown `MAVERICK_FOO` gets one warning with its name and no value,
/// a known setting gets none, and the command still runs past the configuration.
#[test]
fn an_unknown_setting_warns_by_name_and_startup_goes_on() {
    let mut env = required();
    env.push(("MAVERICK_FOO".to_owned(), "super-secret-value".to_owned()));
    env.push(("MAVERICK_RATELIMIT_RPM".to_owned(), "300".to_owned()));
    let lines = migrate(&env, true);
    assert_eq!(
        warnings(&lines),
        [(
            "unknown setting ignored".to_owned(),
            "MAVERICK_FOO".to_owned()
        )],
        "{lines:?}"
    );
    // The configuration loaded, so migrate got as far as the database.
    assert!(
        lines.iter().any(|line| line["msg"] == "migrate failed"),
        "{lines:?}"
    );
    // A caller that passes no environment names (every test of `run`) sees no warning.
    assert_eq!(warnings(&migrate(&env, false)), []);
}

/// D50: the settings required today are enough to start; a new required setting would
/// need a safe default or a major release.
#[test]
fn todays_required_settings_are_enough() {
    for role in ["origin", "replica"] {
        let mut env = required();
        env[0].1 = role.to_owned();
        let config = maverick_config::load_with(|name| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        });
        assert!(config.is_ok(), "{role}: {:?}", config.err());
        assert_eq!(warnings(&migrate(&env, true)), [], "{role}");
    }
}

/// D50 through the real binary: `main` passes the process environment's names, so an
/// unknown setting in the environment gets its warning.
#[test]
fn the_binary_warns_about_an_unknown_setting_in_its_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_maverick"))
        .arg("migrate")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .envs(required())
        .env("MAVERICK_FOO", "super-secret-value")
        .output()
        .expect("run maverick");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(!stdout.contains("super-secret-value"), "{stdout}");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    assert_eq!(
        warnings(&lines),
        [(
            "unknown setting ignored".to_owned(),
            "MAVERICK_FOO".to_owned()
        )],
        "{stdout}"
    );
}

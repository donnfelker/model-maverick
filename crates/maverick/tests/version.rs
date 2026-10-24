//! DIVERGENCE: D50. `maverick --version` prints the release, the build, the schema this
//! binary migrates to, and the bundle formats it imports (spec §14.5). The Go engine had
//! no version flag.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use maverick_cli::version::{BUNDLE_FORMATS, COMMIT, RELEASE_DATE, VERSION, schema_version};
use maverick_cli::{MAIN_USAGE, Output, run};

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

fn invoke(args: &[&str]) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let code = run(
        &args,
        &|_| None,
        Output::new(Shared(out.clone())),
        Output::new(Shared(err.clone())),
    );
    let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
    };
    (code, read(&out), read(&err))
}

#[test]
fn version_prints_five_lines_and_needs_no_configuration() {
    let (code, stdout, stderr) = invoke(&["--version"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr, "");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            format!("maverick {VERSION}"),
            format!("release date: {RELEASE_DATE}"),
            format!("commit: {COMMIT}"),
            format!("minimum schema: {}", schema_version()),
            "bundle formats: mmbundle1".to_owned(),
        ],
        "{stdout}"
    );
    assert!(stdout.ends_with('\n'));
    assert_eq!(BUNDLE_FORMATS, &["mmbundle1"]);
}

#[test]
fn version_reports_the_workspace_release_and_the_newest_migration() {
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    // A development build carries no release values.
    if option_env!("MAVERICK_RELEASE_DATE").is_none() {
        assert_eq!(RELEASE_DATE, "dev");
    }
    if option_env!("MAVERICK_RELEASE_COMMIT").is_none() {
        assert_eq!(COMMIT, "unknown");
    }
    let newest = std::fs::read_dir(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../maverick-store/migrations"
    ))
    .expect("migrations dir")
    .filter_map(|entry| {
        let name = entry
            .expect("entry")
            .file_name()
            .into_string()
            .expect("utf8");
        name.ends_with(".sql").then(|| {
            name.split('_')
                .next()
                .expect("prefix")
                .parse::<i64>()
                .expect("number")
        })
    })
    .max()
    .expect("a migration");
    assert_eq!(schema_version(), newest);
}

#[test]
fn version_takes_no_arguments_and_the_usage_lists_it() {
    let (code, stdout, stderr) = invoke(&["--version", "extra"]);
    assert_eq!(code, 2);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        format!("maverick: --version takes no arguments\n\n{MAIN_USAGE}")
    );
    assert!(MAIN_USAGE.contains(
        "\nFlags:\n  --version   print the release, build, schema, and bundle formats\n"
    ));
}

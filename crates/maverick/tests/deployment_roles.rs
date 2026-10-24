//! Deployment boundaries approved in D49 (spec §14.4).

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use maverick_cli::{Output, run};

#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// DIVERGENCE: D49. Replica refusal precedes task-file or database access.
#[test]
fn replica_refuses_every_corpus_command() {
    for args in [
        vec![
            "corpus",
            "load",
            "--version",
            "v",
            "--dir",
            "/no-such-corpus",
        ],
        vec!["corpus", "activate", "--version", "v"],
        vec!["corpus", "status"],
        vec!["corpus", "calibrate"],
    ] {
        let out = Buffer(Arc::default());
        let err = Buffer(Arc::default());
        let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
        let code = run(
            &args,
            &|name| (name == "MAVERICK_ROLE").then(|| "replica".to_owned()),
            Output::new(out.clone()),
            Output::new(err.clone()),
        );
        assert_eq!(code, 1, "{args:?}");
        assert!(out.0.lock().unwrap().is_empty());
        assert_eq!(
            *err.0.lock().unwrap(),
            b"corpus commands need MAVERICK_ROLE=origin\n"
        );
    }
}

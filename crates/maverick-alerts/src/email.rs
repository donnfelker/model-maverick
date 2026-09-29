//! Email delivery over SMTP (reference Part 3 §4.3).
//!
//! A small SMTP client, so the conversation, the message bytes, and the error prefixes
//! are the Go ones: EHLO, STARTTLS when advertised, AUTH PLAIN only over TLS or to
//! localhost, MAIL FROM, RCPT TO per recipient, DATA, QUIT.

use std::collections::HashSet;
use std::sync::Arc;

use maverick_store::alerts::Alert;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::wire::{base64, percent_decode, rfc1123z};

const DEFAULT_PORT: u16 = 587;

/// A configured email destination.
#[derive(Clone, PartialEq, Eq)]
pub struct Email {
    host: String,
    /// `host:port`.
    addr: String,
    auth: Option<(String, String)>,
    from: String,
    to: Vec<String>,
}

impl std::fmt::Debug for Email {
    /// Never prints the SMTP password (G5).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Email")
            .field("addr", &self.addr)
            .field("user", &self.auth.as_ref().map(|(user, _)| user.as_str()))
            .field("from", &self.from)
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

impl Email {
    /// Parses `smtp://[user:pass@]host[:port]/?from=<addr>&to=<addr>[,<addr>...]`.
    pub fn new(url: &str) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(url).map_err(|err| format!("invalid smtp url: {err}"))?;
        if parsed.scheme() != "smtp" {
            return Err(format!("smtp url scheme {:?}, want smtp", parsed.scheme()));
        }
        let host = parsed.host_str().unwrap_or_default().to_owned();
        if host.is_empty() {
            return Err("smtp url is missing a host".to_owned());
        }
        let port = parsed.port().unwrap_or(DEFAULT_PORT);
        // Go `Query().Get`: the first value of each key, even when it is empty.
        let first = |name: &str| {
            parsed
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        };
        let from = first("from");
        let to: Vec<String> = first("to")
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect();
        if from.is_empty() || to.is_empty() {
            return Err("smtp url must carry ?from= and ?to= addresses".to_owned());
        }
        let username = percent_decode(parsed.username());
        let auth = (!username.is_empty()).then(|| {
            let password = percent_decode(parsed.password().unwrap_or_default());
            (username, password)
        });
        Ok(Self {
            addr: format!("{host}:{port}"),
            host,
            auth,
            from,
            to,
        })
    }

    /// `host:port`.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// True when the URL carried credentials.
    pub fn has_auth(&self) -> bool {
        self.auth.is_some()
    }

    /// The message (the DATA payload before dot-stuffing), with CRLF line endings. The
    /// `subject:` line is the stored jsonb text verbatim (QUIRK A1).
    pub fn message(&self, alert: &Alert) -> String {
        format!(
            "From: {from}\r\nTo: {to}\r\nSubject: [maverick] {severity}: {kind}\r\nDate: {date}\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
             {message}\r\n\r\nalert: {id}\r\nsubject: {subject}\r\n",
            from = self.from,
            to = self.to.join(", "),
            severity = alert.severity,
            kind = alert.kind,
            date = rfc1123z(alert.created_at),
            message = alert.message,
            id = alert.id,
            subject = alert.subject.get(),
        )
    }

    /// Runs one SMTP conversation.
    pub(crate) async fn deliver(&self, alert: &Alert) -> Result<(), String> {
        let tcp = TcpStream::connect(&self.addr)
            .await
            .map_err(|err| format!("smtp dial: {err}"))?;
        let mut session = Session::new(Box::new(tcp));
        session
            .expect(220)
            .await
            .map_err(|err| format!("smtp handshake: {err}"))?;
        let mut ext = session
            .hello()
            .await
            .map_err(|err| format!("smtp handshake: {err}"))?;

        let mut tls = false;
        if ext.contains("STARTTLS") {
            session
                .command("STARTTLS", 220)
                .await
                .map_err(|err| format!("smtp starttls: {err}"))?;
            session = session
                .start_tls(&self.host)
                .await
                .map_err(|err| format!("smtp starttls: {err}"))?;
            tls = true;
            ext = session
                .hello()
                .await
                .map_err(|err| format!("smtp starttls: {err}"))?;
        }

        if let Some((user, password)) = &self.auth {
            if !ext.contains("AUTH") {
                return Err("smtp auth: smtp: server doesn't support AUTH".to_owned());
            }
            if !tls && !is_localhost(&self.host) {
                return Err("smtp auth: unencrypted connection".to_owned());
            }
            let token = base64(format!("\0{user}\0{password}").as_bytes());
            session
                .command(&format!("AUTH PLAIN {token}"), 235)
                .await
                .map_err(|err| format!("smtp auth: {err}"))?;
        }

        let mut mail = format!("MAIL FROM:<{}>", self.from);
        if ext.contains("8BITMIME") {
            mail.push_str(" BODY=8BITMIME");
        }
        if ext.contains("SMTPUTF8") {
            mail.push_str(" SMTPUTF8");
        }
        session
            .command(&mail, 250)
            .await
            .map_err(|err| format!("smtp mail from: {err}"))?;
        for rcpt in &self.to {
            session
                .command_any(&format!("RCPT TO:<{rcpt}>"), &[250, 251])
                .await
                .map_err(|err| format!("smtp rcpt {rcpt}: {err}"))?;
        }
        session
            .command("DATA", 354)
            .await
            .map_err(|err| format!("smtp data: {err}"))?;
        session
            .write(&dot_stuff(&self.message(alert)))
            .await
            .map_err(|err| format!("smtp write: {err}"))?;
        session
            .expect(250)
            .await
            .map_err(|err| format!("smtp close data: {err}"))?;
        let _ = session.command("QUIT", 221).await;
        Ok(())
    }
}

fn is_localhost(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Dot-stuffs the payload and appends the `.` terminator line.
fn dot_stuff(message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 8);
    for line in message.split_inclusive("\r\n") {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
    }
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// One SMTP connection, plain or TLS.
struct Session {
    io: BufReader<Box<dyn Stream>>,
}

impl Session {
    fn new(stream: Box<dyn Stream>) -> Self {
        Self {
            io: BufReader::new(stream),
        }
    }

    async fn write(&mut self, text: &str) -> std::io::Result<()> {
        let stream = self.io.get_mut();
        stream.write_all(text.as_bytes()).await?;
        stream.flush().await
    }

    /// Reads one reply, which may span several `NNN-` lines. Returns the code and the
    /// text lines.
    async fn reply(&mut self) -> Result<(u16, Vec<String>), String> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = self
                .io
                .read_line(&mut line)
                .await
                .map_err(|err| err.to_string())?;
            if n == 0 {
                return Err("unexpected EOF".to_owned());
            }
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            let code: u16 = line
                .get(..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| format!("short response: {line}"))?;
            let more = line.as_bytes().get(3) == Some(&b'-');
            lines.push(line.get(4..).unwrap_or_default().to_owned());
            if !more {
                return Ok((code, lines));
            }
        }
    }

    async fn expect(&mut self, want: u16) -> Result<Vec<String>, String> {
        let (code, lines) = self.reply().await?;
        if code == want {
            Ok(lines)
        } else {
            Err(format!("{code} {}", lines.join("\n")))
        }
    }

    async fn command(&mut self, command: &str, want: u16) -> Result<Vec<String>, String> {
        self.command_any(command, &[want]).await
    }

    async fn command_any(&mut self, command: &str, want: &[u16]) -> Result<Vec<String>, String> {
        self.write(&format!("{command}\r\n"))
            .await
            .map_err(|err| err.to_string())?;
        let (code, lines) = self.reply().await?;
        if want.contains(&code) {
            Ok(lines)
        } else {
            Err(format!("{code} {}", lines.join("\n")))
        }
    }

    /// EHLO, falling back to HELO. Returns the advertised extension keywords.
    async fn hello(&mut self) -> Result<HashSet<String>, String> {
        match self.command("EHLO localhost", 250).await {
            Ok(lines) => Ok(lines
                .iter()
                .skip(1)
                .filter_map(|l| l.split_whitespace().next())
                .map(str::to_ascii_uppercase)
                .collect()),
            Err(_) => {
                self.command("HELO localhost", 250).await?;
                Ok(HashSet::new())
            }
        }
    }

    async fn start_tls(self, host: &str) -> Result<Self, String> {
        use rustls_platform_verifier::BuilderVerifierExt;

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|err| err.to_string())?
            .with_platform_verifier()
            .map_err(|err| err.to_string())?
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|err| err.to_string())?;
        let stream = self.io.into_inner();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await
            .map_err(|err| err.to_string())?;
        Ok(Self::new(Box::new(tls)))
    }
}

#[cfg(test)]
mod tests {
    use super::dot_stuff;

    #[test]
    fn dot_stuffing_doubles_leading_dots_and_terminates() {
        assert_eq!(dot_stuff("a\r\n.b\r\n"), "a\r\n..b\r\n.\r\n");
        assert_eq!(dot_stuff("no newline"), "no newline\r\n.\r\n");
    }
}

//! `doctor --mcp` (#1095). rmcp's client does `initialize`, `notifications/initialized`
//! and `tools/list` over stdio or streamable HTTP. One timeout per server.

use super::carry::{Launch, ServerLaunch};
use colored::Colorize;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
    StreamableHttpPostResponse,
};
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::Serialize;
use std::net::ToSocketAddrs;
use std::time::Duration;

const MAX_REASON: usize = 160;

#[derive(Clone)]
pub(crate) struct McpResult {
    pub host: String,
    pub server: String,
    pub outcome: Outcome,
}

#[derive(Clone)]
pub(crate) enum Outcome {
    Ok { tools: usize },
    Failed { reason: String },
    Skipped { missing: Vec<String> },
}

#[derive(Serialize)]
pub struct McpJson {
    pub host: String,
    pub server: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl McpResult {
    pub(crate) fn failed(&self) -> bool {
        matches!(self.outcome, Outcome::Failed { .. })
    }

    pub(crate) fn to_json(&self) -> McpJson {
        let (status, tools, reason) = match &self.outcome {
            Outcome::Ok { tools } => ("ok", Some(*tools), None),
            Outcome::Failed { reason } => ("failed", None, Some(reason.clone())),
            Outcome::Skipped { missing } => ("skipped", None, Some(skip_reason(missing))),
        };
        McpJson {
            host: self.host.clone(),
            server: self.server.clone(),
            status,
            tools,
            reason,
        }
    }
}

/// `ProtocolVersion::LATEST` dropped `initialize`. This is the newest that has it.
fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("contexere", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
}

pub(crate) fn probe(servers: &[ServerLaunch], timeout: Duration) -> Vec<McpResult> {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return servers
            .iter()
            .map(|server| failed(server, "could not start"))
            .collect();
    };
    rt.block_on(async {
        let mut joins = Vec::with_capacity(servers.len());
        for (index, server) in servers.iter().cloned().enumerate() {
            joins.push(tokio::spawn(async move {
                (index, probe_one(server, timeout).await)
            }));
        }
        let mut slots = vec![None; servers.len()];
        for join in joins {
            if let Ok((index, result)) = join.await {
                slots[index] = Some(result);
            }
        }
        slots.into_iter().flatten().collect()
    })
}

async fn probe_one(server: ServerLaunch, timeout: Duration) -> McpResult {
    let secrets = match &server.launch {
        Launch::Stdio { args, env, .. } => secret_values(args, env, &[]),
        Launch::Http { headers, .. } => secret_values(&[], &[], headers),
        Launch::Failed { reason } => {
            return done(
                &server,
                Outcome::Failed {
                    reason: reason.clone(),
                },
            );
        }
        Launch::Skipped { missing } => {
            return done(
                &server,
                Outcome::Skipped {
                    missing: missing.clone(),
                },
            );
        }
    };
    let work = async {
        match &server.launch {
            Launch::Stdio { command, args, env } => stdio_tools(command, args, env).await,
            Launch::Http { url, headers } => http_tools(url, headers).await,
            Launch::Failed { .. } | Launch::Skipped { .. } => unreachable!(),
        }
    };
    let outcome = match tokio::time::timeout(timeout, work).await {
        Ok(Ok(tools)) => Outcome::Ok { tools },
        Ok(Err(reason)) => Outcome::Failed {
            reason: public_reason(&reason, &secrets),
        },
        Err(_) => Outcome::Failed {
            reason: "timed out".to_string(),
        },
    };
    done(&server, outcome)
}

fn done(server: &ServerLaunch, outcome: Outcome) -> McpResult {
    McpResult {
        host: server.host.clone(),
        server: server.server.clone(),
        outcome,
    }
}

fn failed(server: &ServerLaunch, reason: &str) -> McpResult {
    done(
        server,
        Outcome::Failed {
            reason: reason.to_string(),
        },
    )
}

fn secret_values(
    args: &[String],
    env: &[(String, String)],
    headers: &[(String, String)],
) -> Vec<String> {
    args.iter()
        .cloned()
        .chain(env.iter().map(|(_, v)| v.clone()))
        .chain(headers.iter().map(|(_, v)| v.clone()))
        .filter(|v| v.len() >= 8)
        .collect()
}

async fn stdio_tools(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<usize, String> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .kill_on_drop(true);
    let (transport, _) = TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| format!("could not start ({})", err.kind()))?;
    tool_count(transport).await
}

async fn http_tools(url: &str, headers: &[(String, String)]) -> Result<usize, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut custom = std::collections::HashMap::new();
    for (name, value) in headers {
        let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid header")?;
        let value = http::HeaderValue::from_str(value).map_err(|_| "invalid header")?;
        custom.insert(name, value);
    }
    tool_count(StreamableHttpClientTransport::with_client(
        JsonPost,
        StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom),
    ))
    .await
}

/// HTTP/1.1 POST for the SDK client. reqwest measured 17.3 MiB, over the 15 MiB gate.
#[derive(Clone, Copy)]
struct JsonPost;

#[derive(Debug)]
struct HttpFail;
impl std::fmt::Display for HttpFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection failed")
    }
}
impl std::error::Error for HttpFail {}

struct End;
impl futures_core::Stream for End {
    type Item = Result<sse_stream::Sse, sse_stream::Error>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(None)
    }
}

impl StreamableHttpClient for JsonPost {
    type Error = HttpFail;

    fn post_message(
        &self,
        uri: std::sync::Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<std::sync::Arc<str>>,
        auth_header: Option<String>,
        custom_headers: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
    ) -> impl std::future::Future<
        Output = Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>>,
    > + Send {
        let headers: Vec<(String, String)> = custom_headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();
        async move {
            let body = serde_json::to_vec(&message)?;
            tokio::task::spawn_blocking(move || {
                post_json(
                    &uri,
                    &body,
                    &headers,
                    session_id.as_deref(),
                    auth_header.as_deref(),
                )
            })
            .await?
        }
    }

    async fn delete_session(
        &self,
        _: std::sync::Arc<str>,
        _: std::sync::Arc<str>,
        _: Option<String>,
        _: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        Ok(())
    }

    async fn get_stream(
        &self,
        _: std::sync::Arc<str>,
        _: Option<std::sync::Arc<str>>,
        _: Option<String>,
        _: Option<String>,
        _: std::collections::HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_core::Stream<Item = Result<sse_stream::Sse, sse_stream::Error>> + Send>,
        >,
        StreamableHttpError<Self::Error>,
    > {
        Ok(Box::pin(End))
    }
}

fn post_json(
    url: &str,
    body: &[u8],
    headers: &[(String, String)],
    session: Option<&str>,
    auth: Option<&str>,
) -> Result<StreamableHttpPostResponse, StreamableHttpError<HttpFail>> {
    let (https, host, port, path) = split_url(url).ok_or_else(|| io_err("could not connect"))?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|_| io_err("could not connect"))?
        .next()
        .ok_or_else(|| io_err("could not connect"))?;
    let tcp = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(StreamableHttpError::Io)?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut extra = String::new();
    if let Some(session) = session {
        extra.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    if let Some(auth) = auth {
        extra.push_str(&format!("Authorization: Bearer {auth}\r\n"));
    }
    for (name, value) in headers {
        extra.push_str(name);
        extra.push_str(": ");
        extra.push_str(value);
        extra.push_str("\r\n");
    }
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let (status, response_headers, response_body) = if https {
        exchange_tls(tcp, &host, req.as_bytes(), body)?
    } else {
        exchange_plain(tcp, req.as_bytes(), body)?
    };
    if status == 202 || response_body.is_empty() {
        return Ok(StreamableHttpPostResponse::Accepted);
    }
    let message = serde_json::from_slice(&response_body)?;
    let session = response_headers
        .iter()
        .find_map(|(k, v)| k.eq_ignore_ascii_case("mcp-session-id").then(|| v.clone()));
    Ok(StreamableHttpPostResponse::Json(message, session))
}

fn io_err(msg: &str) -> StreamableHttpError<HttpFail> {
    StreamableHttpError::Io(std::io::Error::other(msg))
}

fn split_url(url: &str) -> Option<(bool, String, u16, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let https = match scheme {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".into()),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), p.parse().ok()?),
        _ => (authority.to_string(), if https { 443 } else { 80 }),
    };
    Some((https, host, port, path))
}

type HttpBytes = Result<(u16, Vec<(String, String)>, Vec<u8>), StreamableHttpError<HttpFail>>;

fn exchange_plain(mut tcp: std::net::TcpStream, head: &[u8], body: &[u8]) -> HttpBytes {
    use std::io::Write;
    tcp.write_all(head)?;
    tcp.write_all(body)?;
    tcp.flush()?;
    read_response(&mut tcp)
}

fn exchange_tls(tcp: std::net::TcpStream, host: &str, head: &[u8], body: &[u8]) -> HttpBytes {
    use std::io::Write;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| io_err("could not connect"))?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .map_err(|_| io_err("could not connect"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name)
        .map_err(|_| io_err("could not connect"))?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    tls.write_all(head)?;
    tls.write_all(body)?;
    tls.flush()?;
    read_response(&mut tls)
}

fn read_response(reader: impl std::io::Read) -> HttpBytes {
    use std::io::{BufRead, Read};
    let mut reader = std::io::BufReader::new(reader);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| io_err("not a valid MCP response"))?;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let mut body = Vec::new();
    if let Some(len) = headers.iter().find_map(|(k, v)| {
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.parse::<usize>().ok())
            .flatten()
    }) {
        body.resize(len.min(1024 * 1024), 0);
        reader.read_exact(&mut body)?;
    } else {
        reader.take(1024 * 1024).read_to_end(&mut body)?;
    }
    Ok((status, headers, body))
}

async fn tool_count<T, E, A>(transport: T) -> Result<usize, String>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let running = client_config()
        .serve(transport)
        .await
        .map_err(|e| e.to_string())?;
    let tools = running.list_all_tools().await.map_err(|e| e.to_string())?;
    Ok(tools.len())
}

pub(crate) fn render(results: &[McpResult]) -> String {
    if results.is_empty() {
        return format!("  {}\n", "none configured".bright_black());
    }
    let host_w = results
        .iter()
        .map(|r| r.host.chars().count())
        .max()
        .unwrap_or(0);
    let name_w = results
        .iter()
        .map(|r| r.server.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for result in results {
        let (label, color, detail) = match &result.outcome {
            Outcome::Ok { tools } => (
                "[OK]",
                colored::Color::Green,
                format!("{tools} {}", if *tools == 1 { "tool" } else { "tools" }),
            ),
            Outcome::Failed { reason } => ("[FAILED]", colored::Color::Red, reason.clone()),
            Outcome::Skipped { missing } => {
                ("[SKIPPED]", colored::Color::Yellow, skip_reason(missing))
            }
        };
        out.push_str(&format!(
            "  {}  {}  {}{detail}\n",
            pad(&result.host, host_w),
            pad(&result.server, name_w),
            format!("{label:<10}").color(color).bold(),
        ));
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    out.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(text.chars().count()),
    ));
    out
}

fn skip_reason(missing: &[String]) -> String {
    if missing.len() == 1 {
        format!("{} is not set", missing[0])
    } else {
        format!("{} are not set", missing.join(", "))
    }
}

/// Error text can echo a value we sent. Only values of 8 characters or more
/// are secrets here, so a mode argument like `exit` cannot rewrite `exited`.
fn public_reason(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    let mut text = text.to_string();
    for secret in secrets {
        text = text.replace(secret, "[REDACTED]");
    }
    let joined = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::new();
    for (i, ch) in joined.chars().enumerate() {
        if i == MAX_REASON {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    if out.is_empty() { "failed".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::carry::Launch;
    use std::time::Instant;

    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn server(name: &str, launch: Launch) -> ServerLaunch {
        ServerLaunch {
            host: "Host".into(),
            server: name.into(),
            launch,
        }
    }

    fn fake() -> std::path::PathBuf {
        static BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        BIN.get_or_init(|| {
            let dir = std::env::temp_dir().join("contexere-doctor-mcp-fake");
            std::fs::create_dir_all(&dir).unwrap();
            let src = dir.join("fake.rs");
            let bin = dir.join(format!("fake{}", std::env::consts::EXE_SUFFIX));
            std::fs::write(&src, FAKE).unwrap();
            let out = std::process::Command::new("rustc")
                .args(["--edition=2024", "-o"])
                .arg(&bin)
                .arg(&src)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            bin
        })
        .clone()
    }

    fn mode(name: &str, env: Vec<(String, String)>) -> ServerLaunch {
        server(
            name,
            Launch::Stdio {
                command: fake().to_string_lossy().into_owned(),
                args: vec![name.into()],
                env,
            },
        )
    }

    #[test]
    fn a_good_server_an_exit_and_a_hang_report_three_lines_within_the_timeout() {
        let secret = "supersecretvalue1234";
        let timeout = Duration::from_millis(750);
        let started = Instant::now();
        let found = probe(
            &[
                mode("ok", Vec::new()),
                mode("exit", Vec::new()),
                mode("hang", Vec::new()),
                mode("leak", vec![("SECRET".into(), secret.into())]),
            ],
            timeout,
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < timeout + Duration::from_secs(3) && elapsed < Duration::from_secs(15),
            "{elapsed:?}"
        );
        let lines: Vec<_> = plain(&render(&found)).lines().map(str::to_string).collect();
        assert!(
            lines[0].contains("[OK]") && lines[0].contains("2 tools"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("[FAILED]") && !lines[1].contains("timed out"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("[FAILED]") && lines[2].contains("timed out"),
            "{lines:?}"
        );
        assert!(
            lines[3].contains("[FAILED]") && !lines[3].contains(secret),
            "{lines:?}"
        );
        assert_ne!(lines[0], lines[1]);
        assert_ne!(lines[1], lines[2]);
    }

    const FAKE: &str = r#"
use std::io::{BufRead, Write};
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("exit") => std::process::exit(2),
        Some("hang") => loop { std::thread::sleep(std::time::Duration::from_secs(60)); },
        Some("leak") => {
            let secret = std::env::var("SECRET").unwrap_or_default();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            let id = id_of(&line);
            println!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32000,\"message\":\"bad {secret}\"}}}}");
        }
        _ => {
            let mut out = std::io::stdout();
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let id = id_of(&line);
                if line.contains("\"initialize\"") {
                    let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"f\",\"version\":\"0\"}}}}}}");
                } else if line.contains("tools/list") {
                    let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"a\",\"inputSchema\":{{}}}},{{\"name\":\"b\",\"inputSchema\":{{}}}}]}}}}");
                }
                let _ = out.flush();
            }
        }
    }
}
fn id_of(line: &str) -> String {
    let Some((_, after)) = line.split_once("\"id\"") else { return "1".into(); };
    let id: String = after.trim_start_matches([':', ' ', '\t']).chars().take_while(|c| c.is_ascii_digit()).collect();
    if id.is_empty() { "1".into() } else { id }
}
"#;
}

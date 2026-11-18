//! `doctor --mcp`: ask each configured server whether it answers (#1095).
//!
//! The handshake is the one the bundled SDK still negotiates. rmcp 3.9's
//! `ProtocolVersion::LATEST` (`2026-07-28`) dropped `initialize` for per-request
//! metadata, and `LATEST_WITH_INITIALIZE` is the version a client must send
//! when the subject is the handshake itself. Method names come from the SDK
//! constants (`initialize`, `notifications/initialized`, `tools/list`), which
//! match the 2025-06-18 schema. stdio frames are newline-delimited JSON, the
//! same framing `rmcp`'s `JsonRpcMessageCodec` writes. HTTP is a POST of one
//! JSON-RPC message with `Accept: application/json, text/event-stream`, then
//! `MCP-Protocol-Version` and `Mcp-Session-Id` on everything after `initialize`.

use super::carry::{Launch, ServerLaunch};
use colored::Colorize;
use rmcp::model::{
    ClientCapabilities, ConstString, Implementation, InitializeRequestParams,
    InitializeResultMethod, InitializedNotificationMethod, JsonRpcVersion2_0,
    ListToolsRequestMethod, ProtocolVersion,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// A response body larger than this is not a tool list we should keep reading.
const MAX_BODY: usize = 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;
/// A hostile `nextCursor` must not page forever inside the server's timeout.
const MAX_TOOL_PAGES: usize = 8;
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

/// Probe every server at once. One hung server then costs its own timeout,
/// not the sum, and the results stay in config order.
pub(crate) fn probe(servers: &[ServerLaunch], timeout: Duration) -> Vec<McpResult> {
    let mut slots: Vec<Option<McpResult>> = (0..servers.len()).map(|_| None).collect();
    thread::scope(|scope| {
        let mut joins = Vec::with_capacity(servers.len());
        for (index, server) in servers.iter().enumerate() {
            joins.push(scope.spawn(move || (index, probe_one(server, timeout))));
        }
        for join in joins {
            if let Ok((index, result)) = join.join() {
                slots[index] = Some(result);
            }
        }
    });
    slots.into_iter().flatten().collect()
}

fn probe_one(server: &ServerLaunch, timeout: Duration) -> McpResult {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(&server.launch, timeout)
    }));
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(_) => Outcome::Failed {
            reason: "probe panicked".to_string(),
        },
    };
    McpResult {
        host: server.host.clone(),
        server: server.server.clone(),
        outcome,
    }
}

fn dispatch(launch: &Launch, timeout: Duration) -> Outcome {
    match launch {
        Launch::Failed { reason } => Outcome::Failed {
            reason: reason.clone(),
        },
        Launch::Skipped { missing } => Outcome::Skipped {
            missing: missing.clone(),
        },
        Launch::Stdio { command, args, env } => probe_stdio(command, args, env, timeout),
        Launch::Http { url, headers } => probe_http(url, headers, timeout),
    }
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
        let host = pad(&result.host, host_w);
        let name = pad(&result.server, name_w);
        let detail = detail(&result.outcome);
        out.push_str(&format!(
            "  {host}  {name}  {}{detail}\n",
            status_tag(&result.outcome)
        ));
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let extra = width.saturating_sub(text.chars().count());
    let mut out = text.to_string();
    out.extend(std::iter::repeat_n(' ', extra));
    out
}

fn status_tag(outcome: &Outcome) -> colored::ColoredString {
    let (label, paint) = match outcome {
        Outcome::Ok { .. } => ("[OK]", colored::Color::Green),
        Outcome::Failed { .. } => ("[FAILED]", colored::Color::Red),
        Outcome::Skipped { .. } => ("[SKIPPED]", colored::Color::Yellow),
    };
    format!("{label:<10}").color(paint).bold()
}

fn detail(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok { tools } => {
            let word = if *tools == 1 { "tool" } else { "tools" };
            format!("{tools} {word}")
        }
        Outcome::Failed { reason } => reason.clone(),
        Outcome::Skipped { missing } => skip_reason(missing),
    }
}

fn skip_reason(missing: &[String]) -> String {
    if missing.len() == 1 {
        format!("{} is not set", missing[0])
    } else {
        format!("{} are not set", missing.join(", "))
    }
}

fn probe_stdio(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    timeout: Duration,
) -> Outcome {
    let secrets: Vec<String> = env
        .iter()
        .map(|(_, value)| value.clone())
        .chain(args.iter().cloned())
        .collect();
    let mut cmd = Command::new(command);
    cmd.args(args)
        .envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return failed(format!("could not start ({})", err.kind()), &secrets),
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        stop(&mut child);
        return failed("could not start".to_string(), &secrets);
    };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut reader = BufReader::new(stdout);
            speak(&mut reader, &mut std::io::BufWriter::new(stdin))
        }));
        let _ = tx.send(sent);
    });
    let outcome = match rx.recv_timeout(timeout) {
        Ok(Ok(Ok(tools))) => Outcome::Ok { tools },
        Ok(Ok(Err(reason))) => {
            let reason = if reason == "exited before answering" {
                exit_reason(&mut child)
            } else {
                reason
            };
            failed(reason, &secrets)
        }
        Ok(Err(_)) => failed("probe panicked".to_string(), &secrets),
        Err(mpsc::RecvTimeoutError::Timeout) => failed("timed out".to_string(), &secrets),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            failed("exited before answering".to_string(), &secrets)
        }
    };
    stop(&mut child);
    outcome
}

fn exit_reason(child: &mut Child) -> String {
    match child.try_wait() {
        Ok(Some(status)) => match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "exited before answering".to_string(),
        },
        _ => "exited before answering".to_string(),
    }
}

fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid != 0 {
            // The child leads its own process group, so a server that forked
            // (npx into node) dies with the timeout instead of being orphaned.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// initialize, then notifications/initialized, then tools/list. A server that
/// rejects the first protocol version gets one retry with a version it listed.
fn speak(reader: &mut impl BufRead, writer: &mut impl Write) -> Result<usize, String> {
    let mut version = ProtocolVersion::LATEST_WITH_INITIALIZE;
    let mut id = 1_i64;
    let first = roundtrip(reader, writer, &initialize_message(id, &version))?;
    let init = if let Some(message) = first.error_message().map(str::to_string) {
        match retry_version(&first, &version) {
            Some(next) => {
                version = next;
                id += 1;
                let again = roundtrip(reader, writer, &initialize_message(id, &version))?;
                if again.error_message().is_some() {
                    return Err(rejected("initialize", &message));
                }
                again
            }
            None => return Err(rejected("initialize", &message)),
        }
    } else {
        first
    };
    if init.result.is_none() {
        return Err("not a valid MCP response".to_string());
    }
    send(writer, &initialized_notification())?;
    let mut tools = 0_usize;
    let mut cursor = None;
    for _ in 0..MAX_TOOL_PAGES {
        id += 1;
        let reply = roundtrip(reader, writer, &tools_list_message(id, cursor.as_deref()))?;
        if let Some(message) = reply.error_message() {
            return Err(rejected("tools/list", message));
        }
        let Some(result) = reply.result else {
            return Err("not a valid MCP response".to_string());
        };
        let page = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| "not a valid MCP response".to_string())?;
        tools += page.len();
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty() && cursor.chars().all(|c| !c.is_control()))
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

fn roundtrip(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    message: &Value,
) -> Result<RpcMessage, String> {
    send(writer, message)?;
    let id = message.get("id").and_then(Value::as_i64);
    match id {
        Some(id) => read_response(reader, id),
        None => Ok(RpcMessage::empty()),
    }
}

fn send(writer: &mut impl Write, message: &Value) -> Result<(), String> {
    let mut line =
        serde_json::to_vec(message).map_err(|_| "not a valid MCP response".to_string())?;
    if line.contains(&b'\n') {
        return Err("not a valid MCP response".to_string());
    }
    line.push(b'\n');
    writer
        .write_all(&line)
        .map_err(|_| "exited before answering".to_string())?;
    writer
        .flush()
        .map_err(|_| "exited before answering".to_string())?;
    Ok(())
}

fn read_response(reader: &mut impl BufRead, id: i64) -> Result<RpcMessage, String> {
    for _ in 0..100 {
        let Some(line) = read_line_limited(reader, MAX_BODY)? else {
            return Err("exited before answering".to_string());
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(message) = message_for_id(&value, id) {
            return Ok(message);
        }
    }
    Err("not a valid MCP response".to_string())
}

fn read_line_limited(reader: &mut impl BufRead, cap: usize) -> Result<Option<String>, String> {
    let mut buf = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(io_reason(&err)),
        };
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        let split = available.iter().position(|byte| *byte == b'\n');
        let take = split.map_or(available.len(), |index| index + 1);
        if buf.len().saturating_add(take) > cap {
            return Err("response too large".to_string());
        }
        buf.extend_from_slice(&available[..take]);
        let saw_newline = split.is_some();
        reader.consume(take);
        if saw_newline {
            break;
        }
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

#[derive(Clone)]
struct RpcMessage {
    result: Option<Value>,
    error_code: Option<i64>,
    error_message: Option<String>,
    error_data: Option<Value>,
}

impl RpcMessage {
    fn empty() -> Self {
        Self {
            result: None,
            error_code: None,
            error_message: None,
            error_data: None,
        }
    }

    fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }
}

fn message_for_id(value: &Value, id: i64) -> Option<RpcMessage> {
    let found = value.get("id")?;
    let matches = found.as_i64() == Some(id)
        || found.as_str().and_then(|text| text.parse::<i64>().ok()) == Some(id);
    if !matches {
        return None;
    }
    if let Some(error) = value.get("error") {
        return Some(RpcMessage {
            result: None,
            error_code: error.get("code").and_then(Value::as_i64),
            error_message: Some(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("request rejected")
                    .to_string(),
            ),
            error_data: error.get("data").cloned(),
        });
    }
    Some(RpcMessage {
        result: value.get("result").cloned(),
        error_code: None,
        error_message: None,
        error_data: None,
    })
}

fn retry_version(message: &RpcMessage, requested: &ProtocolVersion) -> Option<ProtocolVersion> {
    if message.error_code
        != Some(i64::from(
            rmcp::model::ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0,
        ))
    {
        return None;
    }
    let supported = message.error_data.as_ref()?.get("supported")?.as_array()?;
    supported
        .iter()
        .filter_map(|value| serde_json::from_value::<ProtocolVersion>(value.clone()).ok())
        .filter(|version| version.has_initialize() && version != requested)
        .max_by(|left, right| left.as_str().cmp(right.as_str()))
}

fn rejected(stage: &str, message: &str) -> String {
    if message.is_empty() {
        format!("{stage} rejected")
    } else {
        format!("{stage} rejected: {message}")
    }
}

fn initialize_message(id: i64, version: &ProtocolVersion) -> Value {
    let params = InitializeRequestParams::new(
        ClientCapabilities::default(),
        Implementation::new("contexere", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(version.clone());
    json!({
        "jsonrpc": JsonRpcVersion2_0.as_str(),
        "id": id,
        "method": InitializeResultMethod.as_str(),
        "params": params,
    })
}

fn initialized_notification() -> Value {
    json!({
        "jsonrpc": JsonRpcVersion2_0.as_str(),
        "method": InitializedNotificationMethod.as_str(),
    })
}

fn tools_list_message(id: i64, cursor: Option<&str>) -> Value {
    let mut message = json!({
        "jsonrpc": JsonRpcVersion2_0.as_str(),
        "id": id,
        "method": ListToolsRequestMethod.as_str(),
    });
    if let Some(cursor) = cursor {
        message["params"] = json!({ "cursor": cursor });
    }
    message
}

fn probe_http(url: &str, headers: &[(String, String)], timeout: Duration) -> Outcome {
    let secrets: Vec<String> = headers.iter().map(|(_, value)| value.clone()).collect();
    if let Err(reason) = check_headers(headers) {
        return failed(reason, &secrets);
    }
    let deadline = Instant::now() + timeout;
    let mut version = ProtocolVersion::LATEST_WITH_INITIALIZE;
    let mut id = 1_i64;
    let init = match post(
        url,
        headers,
        &initialize_message(id, &version),
        None,
        None,
        deadline,
    ) {
        Ok(exchange) => exchange,
        Err(reason) => return failed(reason, &secrets),
    };
    let init_message = match init.message_for() {
        Ok(message) => message,
        Err(reason) => return failed(reason, &secrets),
    };
    let init_message = if let Some(message) = init_message.error_message().map(str::to_string) {
        match retry_version(&init_message, &version) {
            Some(next) => {
                version = next;
                id += 1;
                let again = match post(
                    url,
                    headers,
                    &initialize_message(id, &version),
                    None,
                    None,
                    deadline,
                ) {
                    Ok(exchange) => exchange,
                    Err(reason) => return failed(reason, &secrets),
                };
                match again.message_for() {
                    Ok(message) if message.error_message().is_none() => message,
                    Ok(message) => {
                        return failed(
                            rejected(
                                "initialize",
                                message.error_message().unwrap_or("request rejected"),
                            ),
                            &secrets,
                        );
                    }
                    Err(reason) => return failed(reason, &secrets),
                }
            }
            None => return failed(rejected("initialize", &message), &secrets),
        }
    } else {
        init_message
    };
    let Some(result) = init_message.result else {
        return failed("not a valid MCP response".to_string(), &secrets);
    };
    let negotiated = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .and_then(header_safe_version)
        .map(|text| {
            serde_json::from_value(Value::String(text.to_string())).unwrap_or(version.clone())
        })
        .unwrap_or(version);
    let session = init.session;
    if let Err(reason) = post(
        url,
        headers,
        &initialized_notification(),
        Some(&negotiated),
        session.as_deref(),
        deadline,
    ) {
        return failed(reason, &secrets);
    }
    let mut tools = 0_usize;
    let mut cursor = None;
    for _ in 0..MAX_TOOL_PAGES {
        id += 1;
        let exchange = match post(
            url,
            headers,
            &tools_list_message(id, cursor.as_deref()),
            Some(&negotiated),
            session.as_deref(),
            deadline,
        ) {
            Ok(exchange) => exchange,
            Err(reason) => return failed(reason, &secrets),
        };
        let message = match exchange.message_for() {
            Ok(message) => message,
            Err(reason) => return failed(reason, &secrets),
        };
        if let Some(text) = message.error_message() {
            return failed(rejected("tools/list", text), &secrets);
        }
        let Some(result) = message.result else {
            return failed("not a valid MCP response".to_string(), &secrets);
        };
        let Some(page) = result.get("tools").and_then(Value::as_array) else {
            return failed("not a valid MCP response".to_string(), &secrets);
        };
        tools += page.len();
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty() && cursor.chars().all(|c| !c.is_control()))
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Outcome::Ok { tools }
}

struct Exchange {
    session: Option<String>,
    messages: Vec<RpcMessage>,
    status: u16,
}

impl Exchange {
    /// Messages were already filtered to the request id.
    fn message_for(&self) -> Result<RpcMessage, String> {
        if let Some(message) = self.messages.first() {
            return Ok(message.clone());
        }
        if matches!(self.status, 200 | 202 | 204) {
            return Ok(RpcMessage::empty());
        }
        if self.status >= 400 {
            return Err(format!("HTTP {}", self.status));
        }
        Err("not a valid MCP response".to_string())
    }
}

/// `id` is carried on the JSON-RPC envelope, which `message_for_id` already
/// filtered. An exchange stores only the messages for the request we sent.
fn post(
    url: &str,
    headers: &[(String, String)],
    message: &Value,
    version: Option<&ProtocolVersion>,
    session: Option<&str>,
    deadline: Instant,
) -> Result<Exchange, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("timed out".to_string());
    }
    let url = parse_url(url)?;
    let body = serde_json::to_vec(message).map_err(|_| "not a valid MCP response".to_string())?;
    let request = build_request(&url, headers, &body, version, session)?;
    let tcp = connect(&url, left)?;
    let raw = if url.https {
        let tls = tls_wrap(tcp, &url.host)?;
        exchange(tls, &request)?
    } else {
        exchange(tcp, &request)?
    };
    let content_type = header_value(&raw.headers, "content-type").unwrap_or("");
    let messages = json_messages(content_type, &raw.body)?;
    let wanted = message.get("id").and_then(Value::as_i64);
    if wanted.is_none() && raw.status >= 400 {
        return Err(format!("HTTP {}", raw.status));
    }
    let messages = match wanted {
        Some(id) => messages
            .iter()
            .filter_map(|value| message_for_id(value, id))
            .collect(),
        None => Vec::new(),
    };
    Ok(Exchange {
        session: session_of(&raw.headers),
        messages,
        status: raw.status,
    })
}

fn json_messages(content_type: &str, body: &[u8]) -> Result<Vec<Value>, String> {
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(body);
    if content_type
        .to_ascii_lowercase()
        .contains("text/event-stream")
    {
        return Ok(sse_values(&text));
    }
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(value) => Ok(vec![value]),
        Err(_) => Err("not a valid MCP response".to_string()),
    }
}

fn sse_values(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut data = String::new();
    let flush = |data: &mut String, out: &mut Vec<Value>| {
        if data.is_empty() {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            out.push(value);
        }
        data.clear();
    };
    for line in body.lines() {
        if line.is_empty() {
            flush(&mut data, &mut out);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start_matches(' '));
        }
    }
    flush(&mut data, &mut out);
    out
}

struct RawHttp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct ParsedUrl {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(raw: &str) -> Result<ParsedUrl, String> {
    let (scheme, rest) = raw.split_once("://").ok_or("could not connect")?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err("could not connect".to_string()),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => match rest.split_once('?') {
            Some((authority, query)) => (authority, format!("/?{query}")),
            None => (rest, "/".to_string()),
        },
    };
    // Userinfo is a credential. It is not sent and not printed.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(host) = authority.strip_prefix('[') {
        let (host, rest) = host.split_once(']').ok_or("could not connect")?;
        let port = if let Some(port) = rest.strip_prefix(':') {
            port.parse::<u16>()
                .map_err(|_| "could not connect".to_string())?
        } else if rest.is_empty() {
            if https { 443 } else { 80 }
        } else {
            return Err("could not connect".to_string());
        };
        (host.to_string(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':')
        && port.chars().all(|c| c.is_ascii_digit())
    {
        (
            host.to_string(),
            port.parse::<u16>()
                .map_err(|_| "could not connect".to_string())?,
        )
    } else {
        (authority.to_string(), if https { 443 } else { 80 })
    };
    if host.is_empty() {
        return Err("could not connect".to_string());
    }
    Ok(ParsedUrl {
        https,
        host,
        port,
        path,
    })
}

fn check_headers(headers: &[(String, String)]) -> Result<(), String> {
    for (name, value) in headers {
        if !header_name_ok(name) || value.chars().any(|c| c == '\r' || c == '\n' || c == '\0') {
            return Err("invalid header".to_string());
        }
    }
    Ok(())
}

fn header_name_ok(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_graphic() && c != ':')
}

fn reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept"
            | "content-type"
            | "content-length"
            | "host"
            | "connection"
            | "mcp-protocol-version"
            | "mcp-session-id"
    )
}

fn build_request(
    url: &ParsedUrl,
    headers: &[(String, String)],
    body: &[u8],
    version: Option<&ProtocolVersion>,
    session: Option<&str>,
) -> Result<Vec<u8>, String> {
    let host = if url.host.contains(':') {
        format!("[{}]", url.host)
    } else {
        url.host.clone()
    };
    let host = if (url.https && url.port == 443) || (!url.https && url.port == 80) {
        host
    } else {
        format!("{host}:{}", url.port)
    };
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n",
        path = url.path,
        len = body.len(),
    );
    if let Some(version) = version.and_then(|version| header_safe_version(version.as_str())) {
        request.push_str("MCP-Protocol-Version: ");
        request.push_str(version);
        request.push_str("\r\n");
    }
    if let Some(session) = session.filter(|session| {
        !session.is_empty() && session.len() <= 200 && session.chars().all(|c| c.is_ascii_graphic())
    }) {
        request.push_str("Mcp-Session-Id: ");
        request.push_str(session);
        request.push_str("\r\n");
    }
    for (name, value) in headers {
        if reserved_header(name) {
            continue;
        }
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

fn header_safe_version(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.len() == 10
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'-')
    {
        Some(text)
    } else {
        None
    }
}

fn connect(url: &ParsedUrl, timeout: Duration) -> Result<TcpStream, String> {
    let addrs = lookup(&url.host, url.port, timeout)?;
    let started = Instant::now();
    let mut last = "could not connect".to_string();
    for addr in addrs {
        let left = timeout.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err("timed out".to_string());
        }
        match TcpStream::connect_timeout(&addr, left) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(left));
                let _ = stream.set_write_timeout(Some(left));
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(err) if timed_out(&err) => return Err("timed out".to_string()),
            Err(_) => last = "could not connect".to_string(),
        }
    }
    Err(last)
}

fn lookup(host: &str, port: u16, timeout: Duration) -> Result<Vec<SocketAddr>, String> {
    let host = host.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let found = (host.as_str(), port)
            .to_socket_addrs()
            .map(|addrs| addrs.collect::<Vec<_>>());
        let _ = tx.send(found);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        Ok(_) => Err("could not connect".to_string()),
        Err(_) => Err("timed out".to_string()),
    }
}

fn tls_wrap(
    tcp: TcpStream,
    host: &str,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, String> {
    let config = tls_config()?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| "could not connect".to_string())?;
    let conn =
        rustls::ClientConnection::new(config, name).map_err(|_| "could not connect".to_string())?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: LazyLock<Result<Arc<rustls::ClientConfig>, String>> = LazyLock::new(|| {
        let provider = rustls::crypto::ring::default_provider();
        let builder = rustls::ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .map_err(|_| "could not connect".to_string())?;
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(Arc::new(
            builder.with_root_certificates(roots).with_no_client_auth(),
        ))
    });
    CONFIG.clone()
}

fn exchange(mut stream: impl Read + Write, request: &[u8]) -> Result<RawHttp, String> {
    stream.write_all(request).map_err(|err| io_reason(&err))?;
    stream.flush().map_err(|err| io_reason(&err))?;
    let mut reader = BufReader::new(stream);
    let (status, headers) = read_http_headers(&mut reader)?;
    let body = read_body(&mut reader, &headers)?;
    Ok(RawHttp {
        status,
        headers,
        body,
    })
}

fn read_http_headers(reader: &mut impl BufRead) -> Result<(u16, Vec<(String, String)>), String> {
    let status_line = read_line_limited(reader, MAX_HEADER)?.ok_or("connection failed")?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("not a valid MCP response")?;
    let mut headers = Vec::new();
    let mut used = status_line.len();
    while let Some(line) = read_line_limited(reader, MAX_HEADER)? {
        if line.is_empty() {
            break;
        }
        used = used.saturating_add(line.len());
        if used > MAX_HEADER {
            return Err("response too large".to_string());
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn read_body(reader: &mut impl BufRead, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    let encoding = header_value(headers, "transfer-encoding").unwrap_or("");
    if encoding.to_ascii_lowercase().contains("chunked") {
        return read_chunked(reader);
    }
    if let Some(length) = header_value(headers, "content-length") {
        let length = length
            .trim()
            .parse::<usize>()
            .map_err(|_| "not a valid MCP response".to_string())?;
        if length > MAX_BODY {
            return Err("response too large".to_string());
        }
        let mut buf = vec![0_u8; length];
        reader.read_exact(&mut buf).map_err(|err| io_reason(&err))?;
        return Ok(buf);
    }
    let mut buf = Vec::new();
    reader
        .take((MAX_BODY as u64) + 1)
        .read_to_end(&mut buf)
        .map_err(|err| io_reason(&err))?;
    if buf.len() > MAX_BODY {
        return Err("response too large".to_string());
    }
    Ok(buf)
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line = read_line_limited(reader, MAX_HEADER)?.ok_or("connection failed")?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| "not a valid MCP response".to_string())?;
        if size == 0 {
            break;
        }
        if body.len().saturating_add(size) > MAX_BODY {
            return Err("response too large".to_string());
        }
        let mut chunk = vec![0_u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|err| io_reason(&err))?;
        body.extend(chunk);
        let _ = read_line_limited(reader, 8)?;
    }
    Ok(body)
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn session_of(headers: &[(String, String)]) -> Option<String> {
    let value = header_value(headers, "mcp-session-id")?;
    if value.is_empty() || value.len() > 200 || value.chars().any(|c| !(c.is_ascii_graphic())) {
        return None;
    }
    Some(value.to_string())
}

fn io_reason(err: &std::io::Error) -> String {
    if timed_out(err) {
        "timed out".to_string()
    } else if err.kind() == std::io::ErrorKind::UnexpectedEof {
        "exited before answering".to_string()
    } else {
        "connection failed".to_string()
    }
}

fn timed_out(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

fn failed(reason: String, secrets: &[String]) -> Outcome {
    let reason = public_reason(&reason, secrets);
    Outcome::Failed {
        reason: if reason.is_empty() {
            "failed".to_string()
        } else {
            reason
        },
    }
}

/// Replace `secret` only where it stands on its own, not inside a longer word.
fn redact_whole(text: String, secret: &str) -> String {
    if secret.is_empty() {
        return text;
    }
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = secret.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        let end = index + needle.len();
        let fits = end <= chars.len() && chars[index..end] == needle[..];
        let before_ok = index == 0 || !chars[index - 1].is_ascii_alphanumeric();
        let after_ok = end >= chars.len() || !chars[end].is_ascii_alphanumeric();
        if fits && before_ok && after_ok {
            out.push_str("[REDACTED]");
            index = end;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

/// Server text can echo a value we just sent. Replace those before the line
/// is shown, longest first so a short value cannot split a longer one. A
/// match inside a longer word is left alone: the argument `exit` must not
/// turn `exited` into a redaction.
fn public_reason(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| secret.len() >= 4)
        .collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    let mut text = text.to_string();
    for secret in secrets {
        text = redact_whole(text, secret);
    }
    let collapsed: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let joined = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (index, ch) in joined.chars().enumerate() {
        if index == MAX_REASON {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::carry::Launch;

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

    fn result(host: &str, server: &str, outcome: Outcome) -> McpResult {
        McpResult {
            host: host.to_string(),
            server: server.to_string(),
            outcome,
        }
    }

    #[test]
    fn initialize_uses_the_sdk_handshake_version_and_methods() {
        let version = ProtocolVersion::LATEST_WITH_INITIALIZE;
        assert!(version.has_initialize());
        assert!(!ProtocolVersion::LATEST.has_initialize());
        let message = initialize_message(1, &version);
        assert_eq!(message["jsonrpc"], "2.0");
        assert_eq!(message["id"], 1);
        assert_eq!(message["method"], "initialize");
        assert_eq!(message["params"]["protocolVersion"], version.as_str());
        assert_eq!(message["params"]["clientInfo"]["name"], "contexere");
        assert!(message["params"]["capabilities"].is_object());
        let note = initialized_notification();
        assert_eq!(note["method"], "notifications/initialized");
        assert!(note.get("id").is_none());
        let list = tools_list_message(2, None);
        assert_eq!(list["method"], "tools/list");
        assert!(list.get("params").is_none());
        let paged = tools_list_message(3, Some("cursor-1"));
        assert_eq!(paged["params"]["cursor"], "cursor-1");
        let line = serde_json::to_string(&message).expect("json");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn an_empty_list_says_none_are_configured() {
        assert_eq!(plain(&render(&[])).trim(), "none configured");
    }

    #[test]
    fn one_line_per_server_names_ok_failed_and_skipped() {
        let text = plain(&render(&[
            result("Claude Code", "good", Outcome::Ok { tools: 2 }),
            result(
                "Claude Code",
                "dead",
                Outcome::Failed {
                    reason: "exited with status 2".to_string(),
                },
            ),
            result(
                "Codex CLI",
                "figma",
                Outcome::Skipped {
                    missing: vec!["FIGMA_TOKEN".to_string()],
                },
            ),
            result(
                "Codex CLI",
                "hub",
                Outcome::Skipped {
                    missing: vec!["A".to_string(), "B".to_string()],
                },
            ),
            result("Claude Code", "solo", Outcome::Ok { tools: 1 }),
        ]));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "{text}");
        assert!(
            lines[0].contains("good") && lines[0].contains("[OK]") && lines[0].contains("2 tools"),
            "{text}"
        );
        assert!(
            lines[1].contains("[FAILED]") && lines[1].contains("exited with status 2"),
            "{text}"
        );
        assert!(
            lines[2].contains("[SKIPPED]") && lines[2].contains("FIGMA_TOKEN is not set"),
            "{text}"
        );
        assert!(lines[3].contains("A, B are not set"), "{text}");
        assert!(
            lines[4].contains("1 tool") && !lines[4].contains("1 tools"),
            "{text}"
        );
        assert_eq!(
            lines[0].find("good"),
            lines[1].find("dead"),
            "names drifted: {text}"
        );
    }

    #[test]
    fn a_reported_reason_loses_values_we_sent() {
        let secret = "supersecretvalue1234";
        let shorter = "abcd";
        let reason = public_reason(
            &format!("bad {secret} and {shorter}\nbearer"),
            &[shorter.to_string(), secret.to_string()],
        );
        assert!(!reason.contains(secret), "{reason}");
        assert!(!reason.contains(shorter), "{reason}");
        assert!(reason.contains("[REDACTED]"), "{reason}");
        assert!(!reason.contains('\n'), "{reason}");
        assert_eq!(
            public_reason("exited with status 2", &["exit".to_string()]),
            "exited with status 2"
        );
    }

    fn launch(server: &str, launch: Launch) -> ServerLaunch {
        ServerLaunch {
            host: "Host".to_string(),
            server: server.to_string(),
            launch,
        }
    }

    fn fake_bin() -> std::path::PathBuf {
        static BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        BIN.get_or_init(compile_fake).clone()
    }

    fn compile_fake() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("contexere-doctor-mcp-fake");
        std::fs::create_dir_all(&dir).expect("tempdir");
        let src = dir.join("fake.rs");
        let bin = dir.join(format!("fake{}", std::env::consts::EXE_SUFFIX));
        let source = r#"
use std::io::{BufRead, Write};
fn main() {
    match std::env::args().nth(1).unwrap_or_else(|| "ok".into()).as_str() {
        "exit" => std::process::exit(2),
        "hang" => loop { std::thread::sleep(std::time::Duration::from_secs(60)); },
        "leak" => leak(),
        "pages" => session(true),
        _ => session(false),
    }
}
fn leak() {
    let secret = std::env::var("SECRET").unwrap_or_default();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    println!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":-32000,\"message\":\"bad {secret}\"}}}}", id_of(&line));
}
fn session(pages: bool) {
    let mut out = std::io::stdout();
    let mut seen = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Ok(path) = std::env::var("CAPTURE") {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
        let id = id_of(&line);
        if line.contains("\"initialize\"") {
            let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"fake\",\"version\":\"0\"}}}}}}");
        } else if line.contains("tools/list") {
            if pages && !seen {
                seen = true;
                let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"one\"}}],\"nextCursor\":\"p2\"}}}}");
            } else if pages {
                let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"two\"}}]}}}}");
            } else {
                let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"a\"}},{{\"name\":\"b\"}}]}}}}");
            }
        }
        let _ = out.flush();
    }
}
fn id_of(line: &str) -> String {
    let Some((_, after)) = line.split_once("\"id\"") else { return "1".into() };
    let after = after.trim_start_matches([':', ' ', '\t']);
    let id: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if id.is_empty() { "1".into() } else { id }
}
"#;
        std::fs::write(&src, source).expect("source");
        let output = Command::new("rustc")
            .args(["--edition=2024", "-o"])
            .arg(&bin)
            .arg(&src)
            .output()
            .expect("rustc");
        assert!(
            output.status.success(),
            "fake server failed to compile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        bin
    }

    fn stdio(mode: &str, extra_env: Vec<(String, String)>) -> Launch {
        Launch::Stdio {
            command: fake_bin().to_string_lossy().into_owned(),
            args: vec![mode.to_string()],
            env: extra_env,
        }
    }

    #[test]
    fn a_good_server_an_exit_and_a_hang_report_three_lines_within_the_timeout() {
        let timeout = Duration::from_millis(750);
        let servers = [
            launch("good", stdio("ok", Vec::new())),
            launch("dead", stdio("exit", Vec::new())),
            launch("hung", stdio("hang", Vec::new())),
        ];
        let started = Instant::now();
        let found = probe(&servers, timeout);
        let elapsed = started.elapsed();
        assert!(
            elapsed < timeout + Duration::from_secs(3),
            "hung server was not cut off: {elapsed:?} against {timeout:?}"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "waited out the hang: {elapsed:?}"
        );
        let text = plain(&render(&found));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(
            lines[0].contains("good") && lines[0].contains("[OK]") && lines[0].contains("2 tools"),
            "{text}"
        );
        assert!(
            lines[1].contains("dead")
                && lines[1].contains("[FAILED]")
                && lines[1].contains("exited with status 2"),
            "{text}"
        );
        assert!(
            lines[2].contains("hung")
                && lines[2].contains("[FAILED]")
                && lines[2].contains("timed out"),
            "{text}"
        );
        assert_ne!(lines[0], lines[1]);
        assert_ne!(lines[1], lines[2]);
    }

    #[test]
    fn the_child_is_sent_initialize_then_initialized_then_tools_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = dir.path().join("sent.txt");
        let servers = [launch(
            "good",
            stdio(
                "ok",
                vec![("CAPTURE".to_string(), capture.display().to_string())],
            ),
        )];
        let found = probe(&servers, Duration::from_secs(3));
        assert!(
            matches!(found[0].outcome, Outcome::Ok { tools: 2 }),
            "{}",
            plain(&render(&found))
        );
        let sent = std::fs::read_to_string(&capture).expect("capture");
        let methods: Vec<&str> = sent
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|value| {
                value
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .map(|method| Box::leak(method.into_boxed_str()) as &str)
            .collect();
        assert_eq!(
            methods,
            ["initialize", "notifications/initialized", "tools/list"],
            "{sent}"
        );
        let init: Value = serde_json::from_str(sent.lines().next().unwrap()).unwrap();
        assert_eq!(
            init["params"]["protocolVersion"],
            ProtocolVersion::LATEST_WITH_INITIALIZE.as_str()
        );
        assert!(!plain(&render(&found)).contains("CAPTURE"));
    }

    #[test]
    fn a_paged_tool_list_is_counted_across_both_pages() {
        let found = probe(
            &[launch("paged", stdio("pages", Vec::new()))],
            Duration::from_secs(3),
        );
        assert!(
            matches!(found[0].outcome, Outcome::Ok { tools: 2 }),
            "{}",
            plain(&render(&found))
        );
    }

    #[test]
    fn a_server_that_echoes_its_env_does_not_get_that_value_printed() {
        let secret = "supersecretvalue1234";
        let found = probe(
            &[launch(
                "leak",
                stdio("leak", vec![("SECRET".to_string(), secret.to_string())]),
            )],
            Duration::from_secs(3),
        );
        let text = plain(&render(&found));
        assert!(text.contains("[FAILED]"), "{text}");
        assert!(!text.contains(secret), "{text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn a_missing_binary_fails_without_panicking() {
        let found = probe(
            &[launch(
                "gone",
                Launch::Stdio {
                    command: "contexere-doctor-mcp-no-such-binary".to_string(),
                    args: Vec::new(),
                    env: Vec::new(),
                },
            )],
            Duration::from_secs(2),
        );
        let text = plain(&render(&found));
        assert!(
            text.contains("[FAILED]") && text.contains("could not start"),
            "{text}"
        );
    }

    #[test]
    fn a_launch_that_failed_to_parse_is_printed_and_not_started() {
        let found = probe(
            &[launch(
                "",
                Launch::Failed {
                    reason: "/tmp/mcp.json does not parse".to_string(),
                },
            )],
            Duration::from_millis(50),
        );
        let text = plain(&render(&found));
        assert!(text.contains("does not parse"), "{text}");
        assert!(text.contains("[FAILED]"), "{text}");
    }

    fn header_is(headers: &[(String, String)], name: &str, value: &str) -> bool {
        headers
            .iter()
            .any(|(key, got)| key.eq_ignore_ascii_case(name) && got == value)
    }

    /// One request in, one response out. `respond` is `None` when the server
    /// should hold the connection open until the test asks it to stop.
    fn http_server(
        handle: impl Fn(&[(String, String)], &str) -> Option<(u16, String, String)>
        + Send
        + Sync
        + 'static,
    ) -> (u16, mpsc::Sender<()>, thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, rx) = mpsc::channel();
        let handle = Arc::new(handle);
        let join = thread::spawn(move || {
            listener.set_nonblocking(true).expect("nonblocking");
            loop {
                if rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                        let Some(request) =
                            read_line_limited(&mut reader, MAX_HEADER).ok().flatten()
                        else {
                            break;
                        };
                        if request.is_empty() {
                            break;
                        }
                        let mut headers = Vec::new();
                        while let Some(line) =
                            read_line_limited(&mut reader, MAX_HEADER).ok().flatten()
                        {
                            if line.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = line.split_once(':') {
                                headers.push((name.trim().to_string(), value.trim().to_string()));
                            }
                        }
                        let body = read_body(&mut reader, &headers).unwrap_or_default();
                        let text = String::from_utf8_lossy(&body).into_owned();
                        let Some((status, extra, payload)) = handle(&headers, &text) else {
                            while rx.try_recv().is_err() {
                                thread::sleep(Duration::from_millis(10));
                            }
                            break;
                        };
                        let response = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{payload}",
                            payload.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (port, tx, join)
    }

    #[test]
    fn an_http_server_is_initialized_and_lists_its_tools() {
        let version = ProtocolVersion::LATEST_WITH_INITIALIZE.as_str().to_string();
        let saw_initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&saw_initialized);
        let (port, stop, join) = http_server(move |headers, body| {
            if body.contains("\"method\":\"initialize\"") {
                assert!(
                    body.contains(&format!("\"protocolVersion\":\"{version}\"")),
                    "{body}"
                );
                Some((
                    200,
                    "Mcp-Session-Id: sess-1\r\n".to_string(),
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"{version}\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"h\",\"version\":\"0\"}}}}}}"
                    ),
                ))
            } else if body.contains("notifications/initialized") {
                assert!(
                    header_is(headers, "mcp-session-id", "sess-1"),
                    "{headers:?}"
                );
                assert!(
                    header_is(headers, "mcp-protocol-version", &version),
                    "{headers:?}"
                );
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Some((202, String::new(), String::new()))
            } else {
                assert!(
                    flag.load(std::sync::atomic::Ordering::SeqCst),
                    "tools/list before initialized"
                );
                assert!(body.contains("tools/list"), "{body}");
                assert!(
                    header_is(headers, "mcp-session-id", "sess-1"),
                    "{headers:?}"
                );
                assert!(
                    header_is(headers, "mcp-protocol-version", &version),
                    "{headers:?}"
                );
                assert!(
                    header_is(headers, "authorization", "Bearer supersecretvalue1234"),
                    "the configured header was not sent: {headers:?}"
                );
                Some((
                    200,
                    String::new(),
                    "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"a\"},{\"name\":\"b\"}]}}".to_string(),
                ))
            }
        });
        let found = probe(
            &[launch(
                "remote",
                Launch::Http {
                    url: format!("http://127.0.0.1:{port}/mcp"),
                    headers: vec![(
                        "Authorization".to_string(),
                        "Bearer supersecretvalue1234".to_string(),
                    )],
                },
            )],
            Duration::from_secs(3),
        );
        let _ = stop.send(());
        let _ = join.join();
        let text = plain(&render(&found));
        assert!(text.contains("[OK]") && text.contains("2 tools"), "{text}");
        assert!(!text.contains("supersecretvalue1234"), "{text}");
        assert!(saw_initialized.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn an_http_server_that_never_answers_is_timed_out() {
        let (port, stop, join) = http_server(|_headers, _body| None);
        let timeout = Duration::from_millis(400);
        let started = Instant::now();
        let found = probe(
            &[launch(
                "slow",
                Launch::Http {
                    url: format!("http://127.0.0.1:{port}/mcp"),
                    headers: Vec::new(),
                },
            )],
            timeout,
        );
        let elapsed = started.elapsed();
        let _ = stop.send(());
        let _ = join.join();
        let text = plain(&render(&found));
        assert!(
            text.contains("[FAILED]") && text.contains("timed out"),
            "{text}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "http hang was not cut off: {elapsed:?}"
        );
    }

    #[test]
    fn sse_and_chunked_bodies_yield_the_json_rpc_message() {
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let values = sse_values(sse);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["id"], 1);
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&raw[..]);
        let (status, headers) = read_http_headers(&mut reader).expect("headers");
        assert_eq!(status, 200);
        let body = read_body(&mut reader, &headers).expect("body");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn json_reports_ok_failed_and_skipped_without_values() {
        let secret = "supersecretvalue1234";
        let row = result(
            "Host",
            "leak",
            Outcome::Failed {
                reason: "initialize rejected: bad [REDACTED]".to_string(),
            },
        );
        let json = serde_json::to_string(&row.to_json()).expect("json");
        assert!(json.contains("\"status\":\"failed\""), "{json}");
        assert!(!json.contains(secret), "{json}");
        let ok = result("Host", "good", Outcome::Ok { tools: 2 }).to_json();
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.tools, Some(2));
        let skipped = result(
            "Host",
            "figma",
            Outcome::Skipped {
                missing: vec!["FIGMA_TOKEN".to_string()],
            },
        )
        .to_json();
        assert_eq!(skipped.status, "skipped");
        assert_eq!(skipped.reason.as_deref(), Some("FIGMA_TOKEN is not set"));
    }
}

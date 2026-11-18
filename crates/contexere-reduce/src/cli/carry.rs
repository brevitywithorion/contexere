//! `contexere carry list`: the MCP servers each installed host has configured, so a
//! new machine knows what to set up (#1092). Names only, never a value: env vars and
//! headers are listed by name, and a URL loses its query and any token-shaped path
//! segment.

use serde::Serialize;
use std::path::Path;

const FLAGS: super::Flags = &[("--json", "Machine-readable JSON output")];

#[derive(Serialize, Debug, PartialEq)]
pub struct ServerRow {
    pub host: String,
    pub server: String,
    /// `stdio`, `http`, `unknown`, or `error` when the file did not parse.
    pub transport: &'static str,
    /// The command, the URL, or for an error row what went wrong.
    pub target: String,
    /// Names of the env vars and headers the server uses, never their values.
    pub env: Vec<String>,
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    super::check_flags("carry", args, FLAGS)?;
    if super::wants_help(args) || args.get(2).map(String::as_str) != Some("list") {
        println!("\ncontexere carry list: the MCP servers your agents use, without secrets\n");
        println!("USAGE:\n  contexere carry list [FLAGS]\n");
        super::print_flags(FLAGS);
        return Ok(());
    }

    let rows: Vec<ServerRow> = crate::agents::all_integrations()
        .iter()
        .filter_map(|host| {
            let (path, key) = host.mcp_config()?;
            Some(rows_for(host.name(), &path, key))
        })
        .flatten()
        .collect();

    if super::has_flag(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

/// The servers in one host's config file. An absent file, or one without `key`,
/// gives no rows; a file that does not parse gives one error row.
pub fn rows_for(host: &str, path: &Path, key: &str) -> Vec<ServerRow> {
    match load_servers(path, key) {
        Loaded::Absent => Vec::new(),
        Loaded::Broken(what) => vec![error_row(host, path, &what)],
        Loaded::Present(servers) => servers
            .iter()
            .filter(|(name, server)| !is_ours(name, server))
            .map(|(name, server)| {
                let (transport, target) = match (url_of(server), command_of(server)) {
                    (Some(url), _) => ("http", bare_url(url)),
                    (None, Some(cmd)) => ("stdio", cmd.to_string()),
                    (None, None) => ("unknown", String::new()),
                };
                ServerRow {
                    host: host.to_string(),
                    server: name.clone(),
                    transport,
                    target,
                    env: env_names(server),
                }
            })
            .collect(),
    }
}

fn error_row(host: &str, path: &Path, what: &str) -> ServerRow {
    ServerRow {
        host: host.to_string(),
        server: String::new(),
        transport: "error",
        target: format!("{} {what}", path.display()),
        env: Vec::new(),
    }
}

enum Loaded {
    Absent,
    /// The `what` of an error row, without the path. The parser's own message is
    /// left out: a TOML error quotes the offending line, which may hold a key.
    Broken(String),
    Present(serde_json::Map<String, serde_json::Value>),
}

fn load_servers(path: &Path, key: &str) -> Loaded {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Absent,
        Err(e) => return Loaded::Broken(format!("could not be read ({})", e.kind())),
    };
    if text.trim().is_empty() {
        return Loaded::Absent;
    }
    let parsed = if path.extension().is_some_and(|e| e == "toml") {
        toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|v| serde_json::to_value(v).ok())
    } else {
        serde_json::from_str(&text)
            .or_else(|_| serde_json::from_str(&crate::agents::plain_json(&text)))
            .ok()
    };
    let Some(config) = parsed else {
        return Loaded::Broken("does not parse".to_string());
    };
    match config.get(key).and_then(|s| s.as_object()) {
        Some(servers) => Loaded::Present(servers.clone()),
        None => Loaded::Absent,
    }
}

/// What `doctor --mcp` can do with one configured server (#1095). Built from the
/// same file `rows_for` reads. Values stay here so the probe can present them,
/// and nowhere else: the row a person sees still carries names only.
pub(crate) struct ServerLaunch {
    pub host: String,
    pub server: String,
    pub launch: Launch,
}

pub(crate) enum Launch {
    /// The file did not parse, or the entry has nothing to connect to.
    Failed { reason: String },
    /// A key the server reads from the environment is not set. Nothing is started.
    Skipped { missing: Vec<String> },
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// One launch per server `rows_for` would list, in the same order, plus an error
/// launch when the file does not parse.
pub(crate) fn targets_for(host: &str, path: &Path, key: &str) -> Vec<ServerLaunch> {
    match load_servers(path, key) {
        Loaded::Absent => Vec::new(),
        Loaded::Broken(what) => vec![ServerLaunch {
            host: host.to_string(),
            server: String::new(),
            launch: Launch::Failed {
                reason: format!("{} {what}", path.display()),
            },
        }],
        Loaded::Present(servers) => servers
            .iter()
            .filter(|(name, server)| !is_ours(name, server))
            .map(|(name, server)| launch_from(host, name, server))
            .collect(),
    }
}

fn launch_from(host: &str, name: &str, server: &serde_json::Value) -> ServerLaunch {
    let mut missing = Vec::new();
    for var in required_env(server) {
        if !env_is_set(&var) {
            missing.push(var);
        }
    }
    let url = url_of(server).map(|url| expand(url, &mut missing));
    let command = command_of(server).map(|cmd| expand(cmd, &mut missing));
    let args: Vec<String> = args_of(server)
        .into_iter()
        .map(|arg| expand(&arg, &mut missing))
        .collect();
    let env = env_pairs(server, &mut missing);
    let headers = header_pairs(server, &mut missing);
    missing.sort();
    missing.dedup();

    let launch = if !missing.is_empty() {
        Launch::Skipped { missing }
    } else if let Some(url) = url.filter(|url| !url.is_empty()) {
        Launch::Http { url, headers }
    } else if let Some(command) = command.filter(|cmd| !cmd.trim().is_empty()) {
        Launch::Stdio { command, args, env }
    } else {
        Launch::Failed {
            reason: "no command or url".to_string(),
        }
    };
    ServerLaunch {
        host: host.to_string(),
        server: name.to_string(),
        launch,
    }
}

/// Env vars the server reads from the process, not values stored in the file.
fn required_env(server: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(list) = server.get("env_vars").and_then(|l| l.as_array()) {
        names.extend(list.iter().filter_map(|n| n.as_str().map(str::to_string)));
    }
    if let Some(name) = server.get("bearer_token_env_var").and_then(|n| n.as_str()) {
        names.push(name.to_string());
    }
    if let Some(obj) = server.get("env_http_headers").and_then(|m| m.as_object()) {
        names.extend(obj.values().filter_map(|v| v.as_str().map(str::to_string)));
    }
    names
}

fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// `${NAME}` is replaced from the environment. An unset name is recorded and the
/// placeholder is left out; the caller skips the server rather than starting it
/// with the literal.
fn expand(input: &str, missing: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut chunks = input.split("${");
    if let Some(first) = chunks.next() {
        out.push_str(first);
    }
    for chunk in chunks {
        let Some((name, after)) = chunk.split_once('}') else {
            out.push_str("${");
            out.push_str(chunk);
            continue;
        };
        if is_env_name(name) {
            match std::env::var(name) {
                Ok(value) if !value.is_empty() => out.push_str(&value),
                _ => missing.push(name.to_string()),
            }
        } else {
            out.push_str("${");
            out.push_str(name);
            out.push('}');
        }
        out.push_str(after);
    }
    out
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn args_of(server: &serde_json::Value) -> Vec<String> {
    let list = server
        .get("args")
        .or_else(|| server.get("command").and_then(|cmd| cmd.get("args")));
    list.and_then(|a| a.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn env_pairs(server: &serde_json::Value, missing: &mut Vec<String>) -> Vec<(String, String)> {
    let Some(obj) = server.get("env").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    obj.iter()
        .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), expand(v, missing))))
        .collect()
}

fn header_pairs(server: &serde_json::Value, missing: &mut Vec<String>) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    for key in ["headers", "http_headers"] {
        if let Some(obj) = server.get(key).and_then(|m| m.as_object()) {
            headers.extend(obj.iter().filter_map(|(name, value)| {
                value.as_str().map(|v| (name.clone(), expand(v, missing)))
            }));
        }
    }
    if let Some(obj) = server.get("env_http_headers").and_then(|m| m.as_object()) {
        for (header, var) in obj {
            let Some(var) = var.as_str() else { continue };
            if let Ok(value) = std::env::var(var)
                && !value.is_empty()
            {
                headers.push((header.clone(), value));
            }
        }
    }
    if let Some(var) = server.get("bearer_token_env_var").and_then(|n| n.as_str())
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        && let Ok(value) = std::env::var(var)
        && !value.is_empty()
    {
        headers.push(("Authorization".to_string(), format!("Bearer {value}")));
    }
    headers
}

fn is_ours(name: &str, server: &serde_json::Value) -> bool {
    name == crate::agents::SERVER_KEY
        || (name == "omni" && command_of(server).is_some_and(crate::agents::runs_omni_binary))
}

/// `command` as a string, or Zed's `{"path": ...}` form.
fn command_of(server: &serde_json::Value) -> Option<&str> {
    let command = server.get("command")?;
    command.as_str().or_else(|| command.get("path")?.as_str())
}

fn url_of(server: &serde_json::Value) -> Option<&str> {
    ["url", "httpUrl", "serverUrl"]
        .iter()
        .find_map(|k| server.get(*k)?.as_str())
}

/// Keys of the env and header maps, plus the env var names Codex lists directly.
fn env_names(server: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for map in ["env", "headers", "http_headers", "env_http_headers"] {
        if let Some(obj) = server.get(map).and_then(|m| m.as_object()) {
            names.extend(obj.keys().cloned());
        }
    }
    // Codex's env_http_headers maps a header to the env var holding its value.
    if let Some(obj) = server.get("env_http_headers").and_then(|m| m.as_object()) {
        names.extend(obj.values().filter_map(|v| v.as_str()).map(String::from));
    }
    if let Some(list) = server.get("env_vars").and_then(|l| l.as_array()) {
        names.extend(list.iter().filter_map(|n| n.as_str()).map(String::from));
    }
    if let Some(name) = server.get("bearer_token_env_var").and_then(|n| n.as_str()) {
        names.push(name.to_string());
    }
    names.sort();
    names.dedup();
    names
}

/// A URL without its credentials: no userinfo, no query or fragment, and a path
/// segment long enough to be a token hidden.
fn bare_url(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let path: Vec<&str> = path
        .split('/')
        .map(|seg| {
            if looks_like_token(seg) {
                "[REDACTED]"
            } else {
                seg
            }
        })
        .collect();
    let mut out = if scheme.is_empty() {
        host.to_string()
    } else {
        format!("{scheme}://{host}")
    };
    if rest.contains('/') {
        out.push('/');
        out.push_str(&path.join("/"));
    }
    out
}

// Length alone: a letters-only or digits-only token is still a token, and hiding a
// long readable path segment costs far less than printing a credential.
fn looks_like_token(seg: &str) -> bool {
    seg.len() >= crate::guard::limits::MIN_SECRET_RUN
}

pub fn render(rows: &[ServerRow]) -> String {
    if rows.is_empty() {
        return "No MCP servers found besides Contexere's own.\n".to_string();
    }
    let host_w = rows.iter().map(|r| r.host.len()).max().unwrap_or(0);
    let name_w = rows.iter().map(|r| r.server.len()).max().unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        let line = format!(
            "{:<host_w$}  {:<name_w$}  {:<7}  {}",
            r.host, r.server, r.transport, r.target
        );
        out.push_str(line.trim_end());
        if !r.env.is_empty() {
            out.push_str(&format!("  env: {}", r.env.join(", ")));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentIntegration;

    fn rows(file: &str, text: &str, key: &str) -> Vec<ServerRow> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(file);
        std::fs::write(&path, text).expect("fixture");
        rows_for("host", &path, key)
    }

    fn summary(rows: &[ServerRow]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                format!(
                    "{} {} {} [{}]",
                    r.server,
                    r.transport,
                    r.target,
                    r.env.join(",")
                )
            })
            .collect()
    }

    fn key_of(host: &dyn AgentIntegration) -> &'static str {
        host.mcp_config().expect("reads MCP config").1
    }

    #[test]
    fn claude_code_lists_its_servers_and_skips_ours() {
        let key = key_of(&crate::agents::ClaudeIntegration);
        let text = r#"{"mcpServers":{
            "contexere":{"command":"/x/contexere","args":["--mcp"]},
            "omni":{"command":"/opt/homebrew/bin/omni"},
            "github":{"command":"npx","args":["-y","@x/github"],"env":{"GITHUB_TOKEN":"v"}},
            "linear":{"type":"http","url":"https://mcp.linear.app/mcp","headers":{"Authorization":"v"}}
        }}"#;
        assert_eq!(
            summary(&rows(".claude.json", text, key)),
            [
                "github stdio npx [GITHUB_TOKEN]",
                "linear http https://mcp.linear.app/mcp [Authorization]"
            ]
        );
    }

    #[test]
    fn codex_lists_its_toml_servers() {
        let key = key_of(&crate::agents::CodexIntegration);
        let text = "[mcp_servers.contexere]\ncommand = \"/x/contexere\"\n\n\
            [mcp_servers.docs]\ncommand = \"docs-mcp\"\nenv_vars = [\"DOCS_KEY\"]\n[mcp_servers.docs.env]\nREGION = \"v\"\n\n\
            [mcp_servers.figma]\nurl = \"https://mcp.figma.com/mcp\"\nbearer_token_env_var = \"FIGMA_TOKEN\"\n\n\
            [mcp_servers.hub]\nurl = \"https://hub.example.com/mcp\"\n[mcp_servers.hub.env_http_headers]\nX-Token = \"HUB_TOKEN\"\n";
        assert_eq!(
            summary(&rows("config.toml", text, key)),
            [
                "docs stdio docs-mcp [DOCS_KEY,REGION]",
                "figma http https://mcp.figma.com/mcp [FIGMA_TOKEN]",
                "hub http https://hub.example.com/mcp [HUB_TOKEN,X-Token]"
            ]
        );
    }

    #[test]
    fn cursor_lists_its_servers() {
        let key = key_of(&crate::agents::CursorIntegration);
        let text = r#"{"mcpServers":{"sentry":{"url":"https://mcp.sentry.dev/sse"}}}"#;
        assert_eq!(
            summary(&rows("mcp.json", text, key)),
            ["sentry http https://mcp.sentry.dev/sse []"]
        );
    }

    #[test]
    fn gemini_lists_http_url_servers() {
        let key = key_of(&crate::agents::GeminiIntegration);
        let text = r#"{"theme":"x","mcpServers":{"search":{"httpUrl":"https://s.example.com/mcp","headers":{"X-Api-Key":"v"}}}}"#;
        assert_eq!(
            summary(&rows("settings.json", text, key)),
            ["search http https://s.example.com/mcp [X-Api-Key]"]
        );
    }

    #[test]
    fn no_secret_value_reaches_either_output() {
        let secret = "sk-ant-api03-9f86d081884c7d659a2feaeb0c55e0f1";
        let text = format!(
            r#"{{"mcpServers":{{
                "a":{{"command":"tool","args":["--key","{secret}"],"env":{{"API_KEY":"{secret}"}}}},
                "b":{{"url":"https://user:{secret}@x.example.com/{secret}/mcp?token={secret}","headers":{{"Authorization":"Bearer {secret}"}}}}
            }}}}"#
        );
        let found = rows("mcp.json", &text, "mcpServers");
        assert_eq!(found.len(), 2);
        let json = serde_json::to_string(&found).expect("json");
        let shown = render(&found);
        for out in [&json, &shown] {
            assert!(!out.contains("9f86d081"), "a secret leaked: {out}");
            assert!(out.contains("API_KEY") && out.contains("Authorization"));
        }
        assert!(
            shown.contains("https://x.example.com/[REDACTED]/mcp"),
            "{shown}"
        );
    }

    #[test]
    fn a_single_class_url_token_is_hidden() {
        let text = r#"{"mcpServers":{
            "a":{"url":"https://x.example.com/mcp/abcdefghijklmnopqrstuvwxyzabcdef"},
            "b":{"url":"https://x.example.com/mcp/12345678901234567890"}
        }}"#;
        let found = rows("mcp.json", text, "mcpServers");
        let json = serde_json::to_string(&found).expect("json");
        for out in [&json, &render(&found)] {
            assert!(!out.contains("abcdefghijklmnop"), "{out}");
            assert!(!out.contains("1234567890123456"), "{out}");
            assert!(
                out.contains("https://x.example.com/mcp/[REDACTED]"),
                "{out}"
            );
        }
    }

    #[test]
    fn a_host_without_mcp_config_prints_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(rows_for("host", &dir.path().join("absent.json"), "mcpServers").is_empty());
        assert!(rows("settings.json", r#"{"theme":"dark"}"#, "mcpServers").is_empty());
        assert!(rows("settings.json", "  \n", "mcpServers").is_empty());
    }

    #[test]
    fn a_config_that_does_not_parse_gives_an_error_row() {
        for (file, text) in [
            ("mcp.json", r#"{"mcpServers":{"a":"#),
            (
                "config.toml",
                "[mcp_servers.a]\ncommand = \"sk-ant-secretvalue123456\" oops\n",
            ),
        ] {
            let found = rows(file, text, "mcpServers");
            assert_eq!(found.len(), 1, "{file}");
            assert_eq!(found[0].transport, "error");
            assert!(found[0].target.ends_with("does not parse"));
            assert!(!found[0].target.contains("secretvalue"));
        }
    }

    #[test]
    fn a_config_with_comments_is_still_read() {
        let text = "{\n  // mine\n  \"context_servers\": {\"pg\": {\"command\": {\"path\": \"pg-mcp\"}},},\n}";
        assert_eq!(
            summary(&rows("settings.json", text, "context_servers")),
            ["pg stdio pg-mcp []"]
        );
    }

    fn launches(file: &str, text: &str, key: &str) -> Vec<ServerLaunch> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(file);
        std::fs::write(&path, text).expect("fixture");
        let before = std::fs::read(&path).expect("read");
        let found = targets_for("host", &path, key);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "probe listing wrote the file"
        );
        found
    }

    #[test]
    fn a_missing_config_lists_no_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(targets_for("host", &dir.path().join("absent.json"), "mcpServers").is_empty());
    }

    #[test]
    fn a_server_without_a_command_or_url_is_a_failed_launch() {
        let text = r#"{"mcpServers":{"bare":{},"blank":{"command":"  "}}}"#;
        let found = launches("mcp.json", text, "mcpServers");
        assert_eq!(found.len(), 2);
        for launch in &found {
            match &launch.launch {
                Launch::Failed { reason } => assert_eq!(reason, "no command or url"),
                Launch::Skipped { .. } | Launch::Stdio { .. } | Launch::Http { .. } => {
                    panic!("{} would be contacted", launch.server);
                }
            }
        }
    }

    #[test]
    fn an_unset_key_skips_the_server_and_names_the_key() {
        let text = r#"{"mcpServers":{
            "figma":{"url":"https://mcp.figma.com/mcp","bearer_token_env_var":"CONTEXERE_DOCTOR_MCP_UNSET_1095"},
            "docs":{"command":"docs-mcp","env_vars":["CONTEXERE_DOCTOR_MCP_UNSET_1095"]},
            "hub":{"url":"https://hub.example.com/mcp","env_http_headers":{"X-Token":"CONTEXERE_DOCTOR_MCP_UNSET_1095"}},
            "tpl":{"command":"tool","args":["--key","${CONTEXERE_DOCTOR_MCP_UNSET_1095}"]}
        }}"#;
        let found = launches("mcp.json", text, "mcpServers");
        assert_eq!(found.len(), 4);
        for launch in &found {
            match &launch.launch {
                Launch::Skipped { missing } => {
                    assert_eq!(missing, &["CONTEXERE_DOCTOR_MCP_UNSET_1095".to_string()]);
                }
                Launch::Failed { .. } | Launch::Stdio { .. } | Launch::Http { .. } => {
                    panic!("{} was not skipped", launch.server);
                }
            }
        }
    }

    #[test]
    fn targets_keep_the_values_the_row_refuses_to_print() {
        let secret = "sk-ant-api03-9f86d081884c7d659a2feaeb0c55e0f1";
        let text = format!(
            r#"{{"mcpServers":{{"a":{{"command":"tool","args":["--key","{secret}"],"env":{{"API_KEY":"{secret}"}}}}}}}}"#
        );
        let found = launches("mcp.json", &text, "mcpServers");
        let Launch::Stdio { command, args, env } = &found[0].launch else {
            panic!("expected a stdio launch");
        };
        assert_eq!(command, "tool");
        assert_eq!(args, &["--key".to_string(), secret.to_string()]);
        assert_eq!(env, &[("API_KEY".to_string(), secret.to_string())]);
        let shown = render(&rows("mcp.json", &text, "mcpServers"));
        assert!(!shown.contains("9f86d081"), "{shown}");
    }

    #[test]
    fn targets_skip_our_own_server() {
        let text = r#"{"mcpServers":{
            "contexere":{"command":"/x/contexere","args":["--mcp"]},
            "omni":{"command":"/opt/homebrew/bin/omni"},
            "github":{"command":"npx"}
        }}"#;
        let found = launches("mcp.json", text, "mcpServers");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].server, "github");
    }
}

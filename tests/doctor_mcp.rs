//! #1095: `doctor --mcp` is the only path that starts configured MCP servers.
//! The binary is driven with a private home so a developer's own servers are
//! never contacted.

use std::process::Command;

fn doctor(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_contexere"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CODEX_HOME", home.join("codex"))
        .env("OMNI_DB_PATH", home.join("t.db"))
        .current_dir(home)
        .output()
        .expect("doctor runs")
}

#[test]
fn no_configured_servers_is_a_quiet_line() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = doctor(home.path(), &["doctor", "--mcp"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("MCP Servers"), "{stdout}");
    assert!(stdout.contains("none configured"), "{stdout}");

    let plain = doctor(home.path(), &["doctor"]);
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(
        !plain_out.contains("MCP Servers"),
        "default doctor started the MCP section: {plain_out}"
    );
}

/// `contexere --mcp` is the server hosts launch. It must stay that, and not
/// become the doctor check, when the flag is the invocation itself.
#[test]
fn the_server_flag_still_starts_the_server() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = doctor(home.path(), &["--mcp"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MCP Server"),
        "contexere --mcp did not run the server: {stderr}"
    );
}

#[test]
fn a_server_without_a_command_is_reported_and_nothing_is_written() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = home.path().join(".claude.json");
    let before = b"{\"mcpServers\":{\"bare\":{}}}";
    std::fs::write(&config, before).expect("config");
    let out = doctor(home.path(), &["doctor", "--mcp"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("no command or url"), "{stdout}");
    assert!(stdout.contains("[FAILED]"), "{stdout}");
    assert_eq!(std::fs::read(&config).expect("read"), before);
}

/// The default run must not execute a configured command. `--mcp` does.
#[cfg(unix)]
#[test]
fn the_flag_is_what_starts_a_configured_server() {
    let home = tempfile::tempdir().expect("tempdir");
    let marker = home.path().join("started");
    let script = home.path().join("mark.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch \"{}\"\nexit 2\n", marker.display()),
    )
    .expect("script");
    std::fs::set_permissions(
        &script,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::write(
        home.path().join(".claude.json"),
        format!(
            "{{\"mcpServers\":{{\"marker\":{{\"command\":\"{}\",\"args\":[\"{}\"]}}}}}}",
            script.display(),
            marker.display()
        ),
    )
    .expect("config");

    let plain = doctor(home.path(), &["doctor"]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(
        !marker.exists(),
        "default doctor started a configured server"
    );

    let out = doctor(home.path(), &["doctor", "--mcp"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(marker.exists(), "doctor --mcp did not start the server");
    assert!(stdout.contains("[FAILED]"), "{stdout}");
    assert!(stdout.contains("exited with status 2"), "{stdout}");
    assert!(
        !stdout.contains(&script.display().to_string()),
        "the command was printed: {stdout}"
    );
}

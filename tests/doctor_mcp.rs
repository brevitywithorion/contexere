//! #1095: `doctor --mcp` is opt-in, and `contexere --mcp` stays the server.
use std::process::Command;

fn run(home: &std::path::Path, args: &[&str]) -> std::process::Output {
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
        .expect("runs")
}

#[test]
fn doctor_mcp_lists_nothing_and_the_bare_flag_is_still_the_server() {
    let home = tempfile::tempdir().expect("tempdir");
    let mcp = run(home.path(), &["doctor", "--mcp"]);
    let stdout = String::from_utf8_lossy(&mcp.stdout);
    assert!(
        mcp.status.success(),
        "{}",
        String::from_utf8_lossy(&mcp.stderr)
    );
    assert!(stdout.contains("none configured"), "{stdout}");
    let plain_out = run(home.path(), &["doctor"]);
    let plain = String::from_utf8_lossy(&plain_out.stdout);
    assert!(!plain.contains("MCP Servers"), "{plain}");
    let server = run(home.path(), &["--mcp"]);
    assert!(
        String::from_utf8_lossy(&server.stderr).contains("MCP Server"),
        "contexere --mcp did not start the server"
    );
}

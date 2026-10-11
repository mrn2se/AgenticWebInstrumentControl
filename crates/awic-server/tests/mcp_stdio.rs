//! End-to-end test of `awic mcp-stdio` (SPECIFICATION.md §8.1) against a
//! real, separately-spawned `awic` server process — not the in-process
//! router `tests/mcp_protocol.rs` exercises. This is the one piece of the
//! MCP transport that a router-level test can't cover: the shim's own
//! hand-rolled HTTP client (`mcp::stdio::post`), reading a real
//! `Content-Length`-framed response off a real `TcpStream`, and carrying
//! the `Mcp-Session-Id` it gets back from `initialize` onto every later
//! request. `[backends] fake = true, native = false` — no hardware
//! involved (DEVELOPMENT.md §1: "no test may require hardware").

use std::net::TcpListener as StdTcpListener;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// Binds an ephemeral port and immediately releases it, for a config file
/// this test writes and a separate process then binds — the same
/// bind-then-drop trick `awic-server`'s own in-process tests avoid needing
/// only because they can bind port 0 and read the real address back from
/// the same `TcpListener`; an external process can't hand that back to us.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn wait_until_connectable(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("server on port {port} never became connectable");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

#[tokio::test]
async fn mcp_stdio_shim_proxies_a_full_session_to_a_real_server() {
    let http_port = free_port();
    let scpi_port = free_port();

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("awic.toml");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(
        &config_path,
        format!(
            "[server]\n\
             bind = \"127.0.0.1:{http_port}\"\n\
             scpi_port = {scpi_port}\n\
             state_dir = {state_dir:?}\n\
             [backends]\n\
             native = false\n\
             fake = true\n"
        ),
    )
    .expect("write test config");

    let awic_bin = env!("CARGO_BIN_EXE_awic");
    let _server = ChildGuard(
        Command::new(awic_bin)
            .env("AWIC_CONFIG", &config_path)
            .env("RUST_LOG", "error")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn awic server"),
    );

    wait_until_connectable(http_port).await;

    let mut shim = ChildGuard(
        Command::new(awic_bin)
            .arg("mcp-stdio")
            .arg("--url")
            .arg(format!("http://127.0.0.1:{http_port}/mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn mcp-stdio"),
    );

    let mut stdin = shim.0.stdin.take().expect("shim stdin");
    let stdout = shim.0.stdout.take().expect("shim stdout");
    let mut lines = BufReader::new(stdout).lines();

    async fn send(stdin: &mut tokio::process::ChildStdin, value: &Value) {
        stdin.write_all(value.to_string().as_bytes()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn recv(lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>) -> Value {
        let line = timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("timed out waiting for a line from mcp-stdio")
            .expect("read line")
            .expect("shim's stdout closed unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("not JSON: {err}: {line:?}"))
    }

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "stdio-test" } }
        }),
    )
    .await;
    let init = recv(&mut lines).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "awic", "{init:?}");

    // A notification gets no reply; the shim must not desync the line
    // stream over it.
    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let list = recv(&mut lines).await;
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "list_instruments"),
        "{list:?}"
    );

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "list_instruments", "arguments": {} }
        }),
    )
    .await;
    let called = recv(&mut lines).await;
    assert_eq!(called["result"]["isError"], false, "{called:?}");
    let devices = called["result"]["structuredContent"]["devices"]
        .as_array()
        .expect("devices array");
    assert!(
        !devices.is_empty(),
        "fake backend must report devices: {called:?}"
    );

    drop(stdin); // close stdin: the shim should exit cleanly on EOF.
    let status = timeout(Duration::from_secs(5), shim.0.wait())
        .await
        .expect("mcp-stdio did not exit after stdin closed")
        .expect("wait on mcp-stdio");
    assert!(status.success(), "mcp-stdio exited with {status:?}");
}

//! End-to-end tests of where the server's settings come from: the config
//! file, `AWIC_<SECTION>__<KEY>` environment variables, then `--set`/`--bind`
//! arguments, each overriding the one before (`awic_server::cli`). Each test
//! spawns the real `awic` binary and checks the setting took effect where it
//! can be seen from outside, which port the server listens on, rather than
//! trusting the parsed struct. `[backends] fake = true, native = false`: no
//! hardware involved (DEVELOPMENT.md §1).
//!
//! Also covers `tools/run.sh`/`tools/run-rootful.sh` handing those variables and
//! arguments on to the container, against a stand-in `podman` that only
//! prints what it was given.

use std::net::TcpListener as StdTcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// An ephemeral port, released for the server to bind (see `mcp_stdio.rs`).
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn connectable(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

async fn wait_until_connectable(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !connectable(port).await {
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

/// A temp dir holding a state dir and, optionally, a config file.
struct Bench {
    dir: tempfile::TempDir,
}

impl Bench {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        Self { dir }
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    /// A config file binding `http_port`/`scpi_port`, fake instruments only.
    fn config(&self, http_port: u16, scpi_port: u16) -> PathBuf {
        let path = self.dir.path().join("awic.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\n\
                 bind = \"127.0.0.1:{http_port}\"\n\
                 scpi_port = {scpi_port}\n\
                 state_dir = {:?}\n\
                 [backends]\n\
                 native = false\n\
                 fake = true\n",
                self.state_dir()
            ),
        )
        .expect("write test config");
        path
    }

    /// The `awic` binary with none of this process's `AWIC_*` variables, so
    /// a developer's own environment cannot leak into a test.
    fn awic(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_awic"));
        for (name, _) in std::env::vars() {
            if name.starts_with("AWIC_") {
                cmd.env_remove(name);
            }
        }
        cmd.env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

/// Runs a command expected to exit by itself; returns (code, stdout+stderr).
async fn run_to_exit(mut cmd: Command) -> (i32, String) {
    let out = timeout(Duration::from_secs(10), cmd.output())
        .await
        .expect("awic should exit by itself")
        .expect("run awic");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

#[tokio::test]
async fn arguments_override_the_environment_which_overrides_the_file() {
    let bench = Bench::new();
    let (file_port, env_port, arg_port) = (free_port(), free_port(), free_port());
    let (file_scpi, env_scpi) = (free_port(), free_port());
    let config = bench.config(file_port, file_scpi);

    let mut cmd = bench.awic();
    cmd.env("AWIC_SERVER__BIND", format!("127.0.0.1:{env_port}"))
        .env("AWIC_SERVER__SCPI_PORT", env_scpi.to_string())
        .args(["--config", config.to_str().unwrap()])
        .args(["--bind", &format!("127.0.0.1:{arg_port}")]);
    let _server = ChildGuard(cmd.spawn().expect("spawn awic"));

    // `--bind` beats the variable and the file; the SCPI port, which only
    // the file and the variable give, comes from the variable.
    wait_until_connectable(arg_port).await;
    wait_until_connectable(env_scpi).await;
    assert!(
        !connectable(env_port).await,
        "env bind should be overridden"
    );
    assert!(
        !connectable(file_port).await,
        "file bind should be overridden"
    );
    assert!(
        !connectable(file_scpi).await,
        "file scpi_port should be overridden"
    );
}

#[tokio::test]
async fn set_overrides_any_setting_and_the_last_one_wins() {
    let bench = Bench::new();
    let (file_port, scpi_port, first, last) = (free_port(), free_port(), free_port(), free_port());
    let config = bench.config(file_port, free_port());

    let mut cmd = bench.awic();
    cmd.args(["-c", config.to_str().unwrap()])
        .args(["--set", &format!("server.scpi_port={scpi_port}")])
        .args(["-s", &format!("server.bind=127.0.0.1:{first}")])
        .arg(format!("--set=server.bind=127.0.0.1:{last}"));
    let _server = ChildGuard(cmd.spawn().expect("spawn awic"));

    wait_until_connectable(last).await;
    wait_until_connectable(scpi_port).await;
    assert!(!connectable(first).await, "an earlier --set should lose");
    assert!(!connectable(file_port).await);
}

/// Before this change, environment overrides were dropped whenever the
/// config file was missing.
#[tokio::test]
async fn the_environment_applies_without_a_config_file() {
    let bench = Bench::new();
    let (http_port, scpi_port) = (free_port(), free_port());

    let mut cmd = bench.awic();
    cmd.env("AWIC_SERVER__BIND", format!("127.0.0.1:{http_port}"))
        .env("AWIC_SERVER__SCPI_PORT", scpi_port.to_string())
        .env("AWIC_SERVER__STATE_DIR", bench.state_dir())
        .env("AWIC_BACKENDS__NATIVE", "false")
        .env("AWIC_BACKENDS__FAKE", "true");
    // No `--config` or `AWIC_CONFIG`: the default path, absent here.
    let _server = ChildGuard(cmd.spawn().expect("spawn awic"));

    wait_until_connectable(http_port).await;
    wait_until_connectable(scpi_port).await;
}

#[tokio::test]
async fn a_misspelled_variable_is_logged_and_the_server_still_starts() {
    let bench = Bench::new();
    let http_port = free_port();
    let config = bench.config(http_port, free_port());

    let mut cmd = bench.awic();
    cmd.env("AWIC_SERVR__BIND", "127.0.0.1:1")
        .env("AWIC_MCP_URL", "http://unrelated") // no `__`: not a setting
        .args(["--config", config.to_str().unwrap()]);
    let mut server = ChildGuard(cmd.spawn().expect("spawn awic"));

    let mut lines = BufReader::new(server.0.stdout.take().unwrap()).lines();
    let mut warned = Vec::new();
    let read_log = async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("names no setting") {
                warned.push(line);
            } else if line.contains("awic-server listening") {
                return;
            }
        }
        panic!("server exited before listening");
    };
    timeout(Duration::from_secs(10), read_log)
        .await
        .expect("server should start");

    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("AWIC_SERVR__BIND"), "{}", warned[0]);
    assert!(connectable(http_port).await);
}

#[tokio::test]
async fn help_prints_usage_and_exits_cleanly() {
    let bench = Bench::new();
    let mut cmd = bench.awic();
    cmd.arg("--help");
    let (code, out) = run_to_exit(cmd).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Usage: awic"), "{out}");
    assert!(out.contains("--set KEY=VALUE"), "{out}");
    assert!(out.contains("AWIC_<SECTION>__<KEY>"), "{out}");
}

#[tokio::test]
async fn bad_arguments_exit_with_usage() {
    for args in [
        &["--frobnicate"][..],
        &["--set"],
        &["--set", "no-equals-sign"],
        &["--bind"],
    ] {
        let bench = Bench::new();
        let mut cmd = bench.awic();
        cmd.args(args);
        let (code, out) = run_to_exit(cmd).await;
        assert_eq!(code, 2, "{args:?}: {out}");
        assert!(out.contains("Usage: awic"), "{args:?}: {out}");
    }
}

#[tokio::test]
async fn an_unknown_or_mistyped_setting_stops_the_server() {
    let bench = Bench::new();
    let config = bench.config(free_port(), free_port());

    let mut cmd = bench.awic();
    cmd.args([
        "-c",
        config.to_str().unwrap(),
        "--set",
        "server.bnd=0.0.0.0:8080",
    ]);
    let (code, out) = run_to_exit(cmd).await;
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("server.bnd"), "{out}");
    assert!(out.contains("bind"), "should list the valid keys: {out}");

    let mut cmd = bench.awic();
    cmd.args([
        "-c",
        config.to_str().unwrap(),
        "--set",
        "server.scpi_port=lots",
    ]);
    let (code, out) = run_to_exit(cmd).await;
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("scpi_port"), "{out}");
}

#[tokio::test]
async fn a_named_config_file_must_exist() {
    let bench = Bench::new();
    let missing = bench.dir.path().join("missing.toml");

    let mut cmd = bench.awic();
    cmd.args(["--config", missing.to_str().unwrap()]);
    let (code, out) = run_to_exit(cmd).await;
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("not found"), "{out}");

    let mut cmd = bench.awic();
    cmd.env("AWIC_CONFIG", &missing);
    let (code, out) = run_to_exit(cmd).await;
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("not found"), "{out}");
}

// ---- tools/run.sh / tools/run-rootful.sh ----

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A `bin/` with a `podman` that prints each argument on its own line, and a
/// `sudo` that just runs its command, for `tools/run-rootful.sh`.
fn fake_tools(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        ("podman", "#!/bin/sh\nprintf '%s\\n' \"$@\"\n"),
        ("sudo", "#!/bin/sh\nexec \"$@\"\n"),
    ] {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// The arguments the script gave `podman run`, one per element.
async fn podman_run_args(script: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let bin = fake_tools(dir.path());
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    let mut cmd = Command::new(repo_root().join(script));
    for (name, _) in std::env::vars() {
        if name.starts_with("AWIC_") {
            cmd.env_remove(name);
        }
    }
    let out = cmd
        .env("PATH", path)
        .env("AWIC_SERVER__BIND", "0.0.0.0:8080")
        .env("AWIC_RECORDER__MEMORY_MB", "512")
        .env("AWIC_SERVER__STATE_DIR", "/host/path")
        .env("AWIC_SERVER__STATIC_DIR", "/host/dist")
        .env("AWIC_CONFIG", "/host/awic.toml")
        .args(["my/image:tag", "--set", "auth.token=a b"])
        .output()
        .await
        .expect("run script");
    assert!(out.status.success(), "{script}: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    // `tools/run-rootful.sh` also calls `podman image exists` first: keep only the
    // `run` invocation.
    let lines: Vec<String> = stdout.lines().map(str::to_string).collect();
    let start = lines.iter().position(|l| l == "run").expect("a podman run");
    lines[start..].to_vec()
}

fn env_flags(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == "-e")
        .map(|w| w[1].clone())
        .collect()
}

#[tokio::test]
async fn run_scripts_forward_settings_and_arguments_to_the_container() {
    for script in ["tools/run.sh", "tools/run-rootful.sh"] {
        let args = podman_run_args(script).await;

        let mut env = env_flags(&args);
        env.sort();
        assert_eq!(
            env,
            [
                "AWIC_RECORDER__MEMORY_MB=512",
                "AWIC_SERVER__BIND=0.0.0.0:8080"
            ],
            "{script}: only settings, and not the host's directories"
        );

        let image = args
            .iter()
            .position(|a| a == "my/image:tag")
            .expect("image");
        assert_eq!(
            args[image + 1..],
            ["--set", "auth.token=a b"],
            "{script}: arguments after the image, intact"
        );
    }
}

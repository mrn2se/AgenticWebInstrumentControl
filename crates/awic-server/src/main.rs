//! axum HTTP/WebSocket server (SPEC §3).

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use awic_core::{Backend, CompositeBackend, Config};
use awic_fake::FakeBackend;
use awic_native::fx2lafw::Fx2lafwBackend;
use awic_native::fx2scope::Fx2ScopeBackend;
use awic_server::scpi::terminal_source::ScpiTerminalSource;
use awic_server::terminal::serial_source::SerialSourceProvider;
use awic_server::terminal::{log_source, TerminalSourceProvider};
use awic_server::usb_real::{spawn_topology_watcher, RealUsbController};
use awic_server::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp-stdio") {
        // `awic mcp-stdio` (SPECIFICATION.md §8.1): a stdio<->HTTP proxy in
        // front of an already-running server's `/mcp` endpoint, not the
        // server itself — no tracing/config/backend setup needed.
        return awic_server::mcp::stdio::run(&args[1..]).await;
    }

    let serve_args = match awic_server::cli::parse(&args) {
        Ok(awic_server::cli::Command::Serve(serve_args)) => serve_args,
        Ok(awic_server::cli::Command::Help) => {
            println!("{}", awic_server::cli::USAGE);
            return Ok(());
        }
        Err(msg) => {
            eprintln!("awic: {msg}\n\n{}", awic_server::cli::USAGE);
            std::process::exit(2);
        }
    };

    let log_source = init_tracing();

    let config = load_config(&serve_args)?;
    let bind = config.server.bind.clone();
    let scpi_bind = scpi_bind_addr(&bind, config.server.scpi_port);

    let usb = Arc::new(RealUsbController::new(std::path::Path::new(
        &config.server.state_dir,
    )));
    let backend = Arc::new(CompositeBackend::new(instrument_backends(&config)));
    let watcher_mode = config.watcher.mode;
    let serial_provider = Arc::new(SerialSourceProvider::new(
        config.serial.sysfs_root.clone(),
        effective_tty_dir(&config),
    ));
    // `ScpiTerminalSource` dispatches through the very `AppState` it's about
    // to be registered into — see `LateAppState`'s doc comment for why this
    // cell exists and when it's safe to assume filled.
    let late_state: awic_server::scpi::terminal_source::LateAppState = Arc::new(OnceLock::new());
    let scpi_terminal_provider = Arc::new(ScpiTerminalSource::new(late_state.clone()));
    let terminal_providers: Vec<Arc<dyn TerminalSourceProvider>> = vec![
        Arc::new(log_source),
        serial_provider,
        scpi_terminal_provider,
    ];
    let state = AppState::with_terminal_providers(config, backend, usb, terminal_providers);
    late_state
        .set(state.clone())
        .unwrap_or_else(|_| unreachable!("late_state is set exactly once, right here"));
    state.bootstrap().await;

    // SCPI-over-TCP (SPECIFICATION.md §9), same host as the WebSocket/HTTP
    // server, its own port. Shuts down alongside everything else via
    // `AppState::subscribe_shutdown` (see `scpi::listener::serve`).
    let scpi_state = state.clone();
    let _scpi_listener = tokio::spawn(async move {
        if let Err(err) = awic_server::scpi::listener::serve(scpi_state, scpi_bind).await {
            tracing::error!(%err, "scpi listener failed");
        }
    });
    let _watcher = spawn_topology_watcher(watcher_mode, state.event_bus().clone());
    // Instruments follow the bus: opened when they appear, dropped when they
    // vanish — including across a port power cycle.
    let _hotplug = tokio::spawn(awic_server::state::run_instrument_hotplug(state.clone()));
    // The terminal module's serial source list changes with the bus too
    // (SPEC §11.2.1): a `usb.changed` event may mean a UART appeared or
    // disappeared, so republish `term.sources` on every one and let clients
    // re-fetch — the same "carries no list, clients re-fetch" shape
    // `usb.changed` itself uses.
    let _term_sources_watch = tokio::spawn(republish_term_sources_on_usb_change(state.clone()));

    // `Option<ConnectInfo<SocketAddr>>` in `ws_handler` (SPEC §12: every
    // mutation is logged with its origin) needs the make-service wired for
    // connect info; a plain `axum::serve(listener, router)` (as this
    // crate's own tests still use) leaves it `None`.
    let router = awic_server::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "awic-server listening");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(state.clone()))
    .await?;

    tracing::info!("awic-server stopped");
    Ok(())
}

/// The SCPI listener's address: `server.bind`'s host, `server.scpi_port`
/// (SPEC §9: "same host as `server.bind`"). `server.bind` is `host:port`
/// (never a bracketed IPv6 literal in any config this project ships), so
/// splitting on the last `:` and swapping the port is exact.
fn scpi_bind_addr(bind: &str, scpi_port: u16) -> String {
    match bind.rsplit_once(':') {
        Some((host, _port)) => format!("{host}:{scpi_port}"),
        None => format!("{bind}:{scpi_port}"),
    }
}

/// `[serial].tty_dir` if set; otherwise `/host/dev` if it exists (the
/// container's bind mount of the host's `/dev`, SPEC §1.3/§11.2), else
/// `/dev` — correct for a `cargo run` directly on the host.
fn effective_tty_dir(config: &Config) -> PathBuf {
    if let Some(configured) = &config.serial.tty_dir {
        return PathBuf::from(configured);
    }
    let host_dev = Path::new("/host/dev");
    if host_dev.exists() {
        host_dev.to_path_buf()
    } else {
        PathBuf::from("/dev")
    }
}

/// Keeps `term.sources` fresh across hot-plug: whenever the bus changes,
/// tell every connected client to re-fetch it (SPEC §11.2.1).
async fn republish_term_sources_on_usb_change(state: AppState) {
    use awic_proto::{Event, PROTOCOL_VERSION};
    let mut events = state.event_bus().subscribe();
    loop {
        match events.recv().await {
            Ok(Event::UsbChanged { .. }) => {
                state.event_bus().publish(Event::TermSourcesChanged {
                    v: PROTOCOL_VERSION,
                });
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// The instrument backends `[backends]` enables: native drivers for real
/// hardware, and optionally the synthetic instruments for development.
fn instrument_backends(config: &Config) -> Vec<Arc<dyn Backend>> {
    let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
    if config.backends.native {
        backends.push(Arc::new(Fx2lafwBackend::new()));
        backends.push(Arc::new(Fx2ScopeBackend::new()));
    }
    if config.backends.fake {
        backends.push(Arc::new(FakeBackend::new()));
    }
    if backends.is_empty() {
        tracing::warn!("no instrument backends enabled in [backends]; no instruments will appear");
    }
    let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
    tracing::info!(?names, "instrument backends");
    backends
}

/// The config from every layer (`awic_server::cli`'s doc): a `--config` or
/// `AWIC_CONFIG` file must exist; the default path may be missing, leaving
/// the defaults with the environment and command line on top.
fn load_config(args: &awic_server::cli::ServeArgs) -> anyhow::Result<Config> {
    let (config_path, required) =
        awic_server::cli::config_path(args, std::env::var("AWIC_CONFIG").ok());
    if !config_path.exists() {
        if required {
            anyhow::bail!("config file {config_path:?} not found");
        }
        tracing::warn!(path = %config_path.display(), "config file not found, using defaults");
    }
    for name in Config::unknown_env_keys() {
        tracing::warn!(%name, "environment variable names no setting; ignored");
    }
    let config = Config::load_layered(Some(&config_path), &args.overrides)
        .map_err(|err| anyhow::anyhow!("failed to load config from {config_path:?}: {err}"))?;
    if !args.overrides.is_empty() {
        let keys: Vec<&str> = args.overrides.iter().map(|(k, _)| k.as_str()).collect();
        tracing::info!(?keys, "settings overridden on the command line");
    }
    Ok(config)
}

/// `tracing` init (DEVELOPMENT.md §1): human-readable on a TTY, JSON
/// otherwise (e.g. under a container runtime collecting logs) — plus the
/// `log` terminal source's own layer (SPEC §12), which always captures
/// `INFO` and above regardless of `RUST_LOG`'s own verbosity, so a quieter
/// console (say, `RUST_LOG=warn`) never silently starves the log terminal of
/// the mutation lines it exists to show. Returns the paired [`LogSource`] to
/// register with the terminal module.
fn init_tracing() -> log_source::LogSource {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let is_tty = std::io::stdout().is_terminal();
    let (log_layer, log_source) = log_source::install();

    let fmt_layer: Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync> =
        if is_tty {
            Box::new(tracing_subscriber::fmt::layer())
        } else {
            Box::new(tracing_subscriber::fmt::layer().json())
        };

    tracing_subscriber::registry()
        .with(fmt_layer.with_filter(filter))
        .with(log_layer.with_filter(LevelFilter::INFO))
        .init();

    log_source
}

/// Waits for SIGINT or SIGTERM, then stops every active capture and tells
/// every open `/ws` connection to close before this future resolves — which
/// is what lets `axum::serve`'s graceful shutdown actually finish promptly
/// instead of waiting indefinitely on long-lived WebSocket connections.
async fn shutdown_signal(state: AppState) {
    use tokio::signal::unix::{signal, SignalKind};

    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, stopping captures and closing sockets");
    state.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scpi_bind_swaps_the_port_but_keeps_the_host() {
        assert_eq!(scpi_bind_addr("127.0.0.1:8080", 5025), "127.0.0.1:5025");
        assert_eq!(scpi_bind_addr("0.0.0.0:8080", 5025), "0.0.0.0:5025");
        assert_eq!(scpi_bind_addr("localhost", 5025), "localhost:5025");
    }
}

//! Configuration (`figment`: TOML + env + command-line overrides), including the two guardrail
//! declarations that make SPECIFICATION.md §4.4 and §7.1 possible: the USB
//! PSU soft limits (USB port protection is derived at runtime, SPEC §4.4).
//!
//! The section names and defaults here (`[server]`, `[auth]`, ...) are
//! deliberately kept in lockstep with the real `config/awic.toml` shipped at
//! the workspace root (built alongside this crate, SPECIFICATION.md §1.2/§4.4)
//! — the test `loads_the_real_repo_config_file` loads it directly so a schema
//! drift between the two fails a unit test rather than surfacing at runtime.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::value::Value;
use figment::Figment;
use serde::{Deserialize, Serialize};

/// Prefix of the environment variables that override settings.
const ENV_PREFIX: &str = "AWIC_";

/// Top-level configuration. Each layer overrides the ones before it: the
/// built-in defaults, a TOML file, `AWIC_`-prefixed environment variables
/// (`AWIC_SERVER__BIND=...`, double underscore separates nesting), then
/// `section.key=value` overrides from the command line ([`Config::load_layered`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub firmware: FirmwareConfig,
    pub psu: PsuConfig,
    pub watcher: WatcherConfig,
    pub renderer: RendererConfig,
    pub backends: BackendsConfig,
    pub recorder: RecorderConfig,
    pub serial: SerialConfig,
    pub mcp: McpConfig,
    /// The removed `[usb]` section, captured only so it can be refused — see
    /// [`Config::reject_removed_sections`].
    #[serde(default, skip_serializing)]
    usb: Option<figment::value::Value>,
}

/// Which instrument backends the server runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BackendsConfig {
    /// Real hardware through the native Rust drivers.
    pub native: bool,
    /// Synthetic instruments on a bus path no real device can occupy — for
    /// developing the UI, MCP and SCPI without hardware attached.
    pub fake: bool,
}

impl Default for BackendsConfig {
    fn default() -> Self {
        Self {
            native: true,
            fake: false,
        }
    }
}

impl Config {
    /// Loads config from `path`, with `AWIC_`-prefixed environment variables
    /// overlaid on top (double underscore nests, e.g. `AWIC_SERVER__BIND`).
    pub fn load(path: &Path) -> Result<Self, Box<figment::Error>> {
        Self::load_layered(Some(path), &[])
    }

    /// Loads config from every layer, lowest precedence first: the built-in
    /// defaults, the TOML file at `path` (if given and present), `AWIC_*`
    /// environment variables, then `overrides` — `("section.key", "value")`
    /// pairs, e.g. from the server's `--set` arguments. Override values parse
    /// like environment values: `true`, `8080`, `[a, b]`, else a string.
    ///
    /// Override keys must name a real setting ([`Config::check_key`]); a
    /// typo would otherwise be silently ignored.
    pub fn load_layered(
        path: Option<&Path>,
        overrides: &[(String, String)],
    ) -> Result<Self, Box<figment::Error>> {
        let mut figment = Figment::new();
        if let Some(path) = path {
            figment = figment.merge(Toml::file(path));
        }
        figment = figment.merge(Env::prefixed(ENV_PREFIX).split("__"));
        for (key, value) in overrides {
            Self::check_key(key).map_err(|msg| Box::new(figment::Error::from(msg)))?;
            let value: Value = value.parse().expect("infallible");
            figment = figment.merge(Serialized::default(key, value));
        }
        let cfg: Self = figment.extract().map_err(Box::new)?;
        cfg.reject_removed_sections()
    }

    /// `Ok` if `key` (`section.key`, or a whole `section`) names a setting.
    pub fn check_key(key: &str) -> Result<(), String> {
        let defaults = Value::serialize(Config::default()).expect("Config serializes");
        let mut node = &defaults;
        for part in key.split('.') {
            let found = node.as_dict().and_then(|dict| dict.get(part));
            match found {
                Some(next) => node = next,
                None => {
                    let known: Vec<&str> = node
                        .as_dict()
                        .map(|dict| dict.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    return Err(if known.is_empty() {
                        format!("unknown setting {key:?}")
                    } else {
                        format!(
                            "unknown setting {key:?}; expected one of: {}",
                            known.join(", ")
                        )
                    });
                }
            }
        }
        Ok(())
    }

    /// `AWIC_*__*` environment variables that name no setting — most likely
    /// typos, since the environment layer itself ignores them. Variables
    /// without a `__` (`AWIC_CONFIG`, `AWIC_MCP_URL`) are not settings and
    /// are left out.
    pub fn unknown_env_keys() -> Vec<String> {
        std::env::vars()
            .filter_map(|(name, _)| {
                let rest = name.strip_prefix(ENV_PREFIX)?;
                if !rest.contains("__") {
                    return None;
                }
                let key = rest.to_ascii_lowercase().replace("__", ".");
                Self::check_key(&key).is_err().then_some(name)
            })
            .collect()
    }

    /// Loads config from an in-memory TOML document — used by tests and by
    /// callers that already have the file contents.
    pub fn from_toml_str(toml: &str) -> Result<Self, Box<figment::Error>> {
        let cfg: Self = Figment::new()
            .merge(Toml::string(toml))
            .extract()
            .map_err(Box::new)?;
        cfg.reject_removed_sections()
    }

    /// Refuses the removed `[usb]` section (`[[usb.switchable]]`). Switchable
    /// ports are discovered from the bus now (SPEC §4.4); silently ignoring an
    /// old list would let a user believe it still limits what can be cut.
    /// Targeted rather than `deny_unknown_fields`, which would also reject
    /// unrelated `AWIC_*` environment variables such as `AWIC_CONFIG`.
    fn reject_removed_sections(self) -> Result<Self, Box<figment::Error>> {
        if self.usb.is_some() {
            return Err(Box::new(figment::Error::from(
                "the [usb] section (usb.switchable) is no longer used: switchable \
                 ports are discovered automatically, and port protection is \
                 changed from the UI or MCP (SPECIFICATION.md §4.4). Remove it."
                    .to_string(),
            )));
        }
        Ok(self)
    }
}

/// `[server]`: HTTP/WS bind address, SCPI port, and the built frontend's
/// location (SPECIFICATION.md §1.1, §9, §11).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    /// `host:port` for the HTTP/WebSocket server. Loopback-only by default;
    /// `0.0.0.0:...` is required for LAN access (SPECIFICATION.md §12).
    pub bind: String,
    /// SCPI-over-TCP port (SPECIFICATION.md §9). Fixed at `5025` per the
    /// spec's own examples; exposed as config in case a bench already uses
    /// that port for something else.
    pub scpi_port: u16,
    /// Directory the built frontend (`vite build` output) is served from.
    pub static_dir: String,
    /// Writable directory for session state the server keeps across restarts
    /// — e.g. the user's USB port lock/unlock choices (SPEC §4.4). Not
    /// configuration: the server owns these files. Defaults to the user's
    /// own state directory ([`default_state_dir`]); the container image sets
    /// `/var/lib/awic`.
    pub state_dir: String,
    /// Browser origins, besides the server's own, allowed to open `/ws` and
    /// `/mcp` (SPECIFICATION.md §12). Browsers apply no same-origin policy to
    /// WebSockets, so without this check any page the user visits could
    /// drive the bench. Defaults to the Vite dev server, whose proxy
    /// rewrites `Host` but forwards the page's `Origin`.
    pub allowed_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            scpi_port: 5025,
            static_dir: "/usr/share/awic/dist".to_string(),
            state_dir: default_state_dir(),
            allowed_origins: vec![
                "http://localhost:5173".to_string(),
                "http://127.0.0.1:5173".to_string(),
            ],
        }
    }
}

/// The default `state_dir`: `$XDG_STATE_HOME/awic`, else
/// `$HOME/.local/state/awic` (the XDG base directory spec), so a server run by
/// a normal user can write it. `/var/lib/awic` only when neither variable is
/// set — the container image sets that path explicitly.
pub fn default_state_dir() -> String {
    state_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn state_dir_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> String {
    // The spec says a relative XDG_STATE_HOME is invalid and to be ignored.
    let xdg = xdg_state_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("awic"));
    let home = home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(".local/state/awic"));
    xdg.or(home)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/var/lib/awic".to_string())
}

/// `[auth]`: the optional bearer token (SPECIFICATION.md §12, R-01). Unset by
/// default — the out-of-the-box behavior is unauthenticated, as specified.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuthConfig {
    pub token: Option<String>,
}

/// SPECIFICATION.md §7.1: PSU soft limits, the only guardrail on setpoints.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PsuConfig {
    pub limits: Vec<PsuLimit>,
}

/// One `[[psu.limits]]` entry. `device = "*"` matches any device not covered
/// by a more specific entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PsuLimit {
    pub device: String,
    pub v_max: f32,
    pub i_max: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FirmwareConfig {
    /// SPECIFICATION.md §13 step 3: opt-in, checksum-verified auto-download.
    /// Off by default — steps 1 (host mount) and 2 (baked into the image)
    /// need no network.
    pub auto_download: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RendererConfig {
    /// SPECIFICATION.md §5.3, D-05: `"webgl2"` is the default (revising
    /// R-06 — Plotly is built for figures, not the streams live mode needs);
    /// `"plotly"` and `"uplot"` are the documented alternatives.
    pub default: String,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            default: "webgl2".to_string(),
        }
    }
}

/// `[recorder]`: the memory budget shared by every recording (SPECIFICATION.md
/// §2.8.4). Over budget, the oldest blocks (and their summary buckets) are
/// evicted from whichever recording holds the globally oldest retained data,
/// until total usage is back at or under this bound.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecorderConfig {
    pub memory_mb: u64,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self { memory_mb: 256 }
    }
}

/// `[mcp]`: the MCP monitor's message log (SPECIFICATION.md §8.5). Sessions
/// and counters are always tracked; the log of requests, responses and the
/// pictures results carried is what costs memory, so it can be switched off
/// (here, or at runtime from the web UI's MCP tab) and is bounded by
/// `log_memory_mb`, oldest entries dropped first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct McpConfig {
    /// Record MCP requests and responses at start.
    pub log: bool,
    /// Also keep the pictures (`render_view` images) results carried.
    pub log_images: bool,
    /// Memory bound for the whole message log, pictures included.
    pub log_memory_mb: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            log: true,
            log_images: true,
            log_memory_mb: 16,
        }
    }
}

/// SPECIFICATION.md §4.3: hot-plug detection strategy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatcherMode {
    /// udev netlink monitor — sub-100ms, requires `--net=host` (§4.3).
    #[default]
    Netlink,
    /// 1 Hz `/sys` polling fallback — ~1s latency, works anywhere.
    Polling,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct WatcherConfig {
    pub mode: WatcherMode,
}

/// `[serial]`: where the terminal module's `serial` source looks for
/// USB-serial interfaces (SPECIFICATION.md §11.2.1). Both directories are
/// configurable because the container sees the host's `/dev` and `/sys`
/// through whatever bind mounts `tools/run.sh`/`tools/run-rootful.sh` set up, which may
/// not sit at the same paths a plain `cargo run` on the host does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SerialConfig {
    /// Root of the sysfs tree to scan for `tty`/`ttyUSB*` nodes under each
    /// USB interface, e.g. `<sysfs_root>/bus/usb/devices/<intf>/tty/ttyACM0`.
    pub sysfs_root: String,
    /// Directory the discovered tty device nodes actually live in. `None`
    /// (the default) picks `/host/dev` if it exists — the container's own
    /// bind mount of the host's `/dev` (SPEC §1.3) — else falls back to
    /// `/dev`, which is correct for a `cargo run` directly on the host.
    pub tty_dir: Option<String>,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            sysfs_root: "/sys".to_string(),
            tty_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_state_dir_is_the_users_own() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            state_dir_from(os("/x/state"), os("/home/u")),
            "/x/state/awic"
        );
        assert_eq!(
            state_dir_from(None, os("/home/u")),
            "/home/u/.local/state/awic"
        );
        // A relative XDG_STATE_HOME is invalid by the spec and ignored.
        assert_eq!(
            state_dir_from(os("rel"), os("/home/u")),
            "/home/u/.local/state/awic"
        );
        assert_eq!(state_dir_from(None, None), "/var/lib/awic");
    }

    #[test]
    fn defaults_are_sane_with_empty_config() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:8080");
        assert_eq!(cfg.server.scpi_port, 5025);
        assert!(cfg.auth.token.is_none());
        assert_eq!(cfg.server.state_dir, default_state_dir());
        assert!(cfg.psu.limits.is_empty());
        assert!(!cfg.firmware.auto_download);
        assert_eq!(cfg.renderer.default, "webgl2");
        assert_eq!(cfg.watcher.mode, WatcherMode::Netlink);
        assert_eq!(cfg.recorder.memory_mb, 256);
        assert_eq!(cfg.serial.sysfs_root, "/sys");
        assert!(cfg.serial.tty_dir.is_none());
    }

    #[test]
    fn serial_section_parses() {
        let toml = r#"
            [serial]
            sysfs_root = "/host/sys"
            tty_dir = "/host/dev"
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert_eq!(cfg.serial.sysfs_root, "/host/sys");
        assert_eq!(cfg.serial.tty_dir.as_deref(), Some("/host/dev"));
    }

    #[test]
    fn parses_limits_matching_spec_example() {
        let toml = r#"
            [[psu.limits]]
            device = "*"
            v_max = 5.5
            i_max = 1.0
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert_eq!(cfg.psu.limits[0].v_max, 5.5);
        assert_eq!(cfg.psu.limits[0].i_max, 1.0);
    }

    #[test]
    fn a_leftover_usb_switchable_section_is_rejected_not_silently_ignored() {
        // Switchable ports are discovered now (SPEC §4.4). An old config that
        // still lists them must fail loudly: silently ignoring it would let a
        // user believe their hand-written list still limits what can be cut.
        let toml = r#"
            [[usb.switchable]]
            hub = "3-3"
            ports = [1]
            label = "x"
        "#;
        assert!(Config::from_toml_str(toml).is_err());
    }

    #[test]
    fn env_override_wins_over_file() {
        // figment's Env provider reads the real process environment, so we
        // exercise it directly rather than through `Config::load` (which
        // also wants a real file path).
        std::env::set_var("AWIC_TEST_RENDERER__DEFAULT", "webgl2");
        let cfg: RendererConfig = Figment::new()
            .merge(Toml::string("default = \"plotly\""))
            .merge(Env::prefixed("AWIC_TEST_RENDERER__").split("__"))
            .extract()
            .unwrap();
        std::env::remove_var("AWIC_TEST_RENDERER__DEFAULT");
        assert_eq!(cfg.default, "webgl2");
    }

    #[test]
    fn overrides_win_over_file_and_parse_like_env_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("awic.toml");
        std::fs::write(
            &path,
            "[server]\nbind = \"127.0.0.1:8080\"\nscpi_port = 5025\n",
        )
        .unwrap();
        let overrides = [
            ("server.bind".to_string(), "0.0.0.0:9090".to_string()),
            ("server.scpi_port".to_string(), "6000".to_string()),
            ("backends.fake".to_string(), "true".to_string()),
            ("auth.token".to_string(), "s3cret".to_string()),
        ];
        let cfg = Config::load_layered(Some(&path), &overrides).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0:9090");
        assert_eq!(cfg.server.scpi_port, 6000);
        assert!(cfg.backends.fake);
        assert_eq!(cfg.auth.token.as_deref(), Some("s3cret"));
    }

    #[test]
    fn overrides_apply_without_a_config_file() {
        let overrides = [("recorder.memory_mb".to_string(), "64".to_string())];
        let missing = Path::new("/nonexistent/awic.toml");
        let cfg = Config::load_layered(Some(missing), &overrides).unwrap();
        assert_eq!(cfg.recorder.memory_mb, 64);
        let cfg = Config::load_layered(None, &overrides).unwrap();
        assert_eq!(cfg.recorder.memory_mb, 64);
    }

    #[test]
    fn an_unknown_override_key_is_refused_with_the_known_ones() {
        let overrides = [("server.bnd".to_string(), "0.0.0.0:8080".to_string())];
        let err = Config::load_layered(None, &overrides)
            .unwrap_err()
            .to_string();
        assert!(err.contains("server.bnd"), "{err}");
        assert!(err.contains("bind"), "{err}");
        assert!(Config::check_key("nosuch").is_err());
        assert!(Config::check_key("server.bind.deeper").is_err());
    }

    #[test]
    fn check_key_knows_every_section_including_unset_options() {
        for key in [
            "server",
            "server.bind",
            "server.allowed_origins",
            "auth.token",
            "serial.tty_dir",
            "psu.limits",
            "watcher.mode",
            "mcp.log_memory_mb",
        ] {
            assert_eq!(Config::check_key(key), Ok(()), "{key}");
        }
        assert!(Config::check_key("usb").is_err());
    }

    #[test]
    fn a_wrongly_typed_override_is_an_error_not_a_default() {
        let overrides = [("server.scpi_port".to_string(), "lots".to_string())];
        assert!(Config::load_layered(None, &overrides).is_err());
    }

    /// The repo ships a real `config/awic.toml` (built by a parallel task,
    /// alongside the `Containerfile` that mounts it at `/etc/awic`). This
    /// test is the contract that this crate's schema actually parses it —
    /// a section rename on either side fails here instead of at boot.
    #[test]
    fn loads_the_real_repo_config_file() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/awic.toml"
        ));
        if !path.exists() {
            // Built by a different, parallel task; don't fail this crate's
            // own test suite if it hasn't landed yet.
            eprintln!("skipping: {path:?} not present");
            return;
        }
        let cfg = Config::load(path).expect("config/awic.toml must parse with this schema");

        assert_eq!(cfg.server.bind, "127.0.0.1:8080");
        assert_eq!(cfg.server.scpi_port, 5025);
        assert_eq!(cfg.server.static_dir, "/usr/share/awic/dist");

        assert!(!cfg.psu.limits.is_empty());
        assert!(!cfg.firmware.auto_download);
    }
}

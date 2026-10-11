//! The server's command line: which config file to read, and settings that
//! override it. Every setting can come from the file, an `AWIC_*`
//! environment variable or `--set`, in rising precedence
//! ([`awic_core::Config::load_layered`]).

use std::path::PathBuf;

/// Where the config file is looked for when neither `--config` nor
/// `AWIC_CONFIG` names one.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/awic/awic.toml";

pub const USAGE: &str = "\
Usage: awic [OPTIONS]
       awic mcp-stdio [--url URL] [--token TOKEN]

Options:
  -c, --config PATH      Config file (default: $AWIC_CONFIG, else /etc/awic/awic.toml)
  -s, --set KEY=VALUE    Override one setting, e.g. --set server.bind=0.0.0.0:8080
                         (repeatable; KEY is section.key as in the config file)
      --bind ADDR        Shorthand for --set server.bind=ADDR
  -h, --help             Show this help

Settings are read from the built-in defaults, then the config file, then
AWIC_<SECTION>__<KEY> environment variables (e.g. AWIC_SERVER__BIND), then
--set/--bind; each overrides the ones before it. Values parse like TOML:
true, 5025, [a, b], else a plain string.";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Run the server.
    Serve(ServeArgs),
    /// Print [`USAGE`] and exit.
    Help,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServeArgs {
    /// `--config`, if given: unlike the default path, it must exist.
    pub config: Option<PathBuf>,
    /// `--set`/`--bind` overrides, in command-line order (later wins).
    pub overrides: Vec<(String, String)>,
}

/// Parses the arguments after the program name (and before any
/// `mcp-stdio` subcommand, which has its own). Accepts `--opt value` and
/// `--opt=value`.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut out = ServeArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg, None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "-c" | "--config" => out.config = Some(PathBuf::from(value("--config")?)),
            "--bind" => out
                .overrides
                .push(("server.bind".to_string(), value("--bind")?)),
            "-s" | "--set" => {
                let pair = value("--set")?;
                let (key, val) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--set expects KEY=VALUE, got {pair:?}"))?;
                let key = key.trim();
                if key.is_empty() {
                    return Err(format!("--set expects KEY=VALUE, got {pair:?}"));
                }
                out.overrides.push((key.to_string(), val.to_string()));
            }
            other => return Err(format!("unknown argument {other:?} (see --help)")),
        }
        i += 1;
    }
    Ok(Command::Serve(out))
}

/// The config file to read and whether it must exist: `--config` first
/// (required), then `AWIC_CONFIG` (required), else [`DEFAULT_CONFIG_PATH`]
/// (optional: a missing file means the defaults).
pub fn config_path(args: &ServeArgs, env: Option<String>) -> (PathBuf, bool) {
    match (&args.config, env) {
        (Some(path), _) => (path.clone(), true),
        (None, Some(path)) => (PathBuf::from(path), true),
        (None, None) => (PathBuf::from(DEFAULT_CONFIG_PATH), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn serve(list: &[&str]) -> ServeArgs {
        match parse(&args(list)).unwrap() {
            Command::Serve(serve) => serve,
            Command::Help => panic!("expected Serve"),
        }
    }

    #[test]
    fn no_arguments_serve_with_nothing_overridden() {
        assert_eq!(serve(&[]), ServeArgs::default());
    }

    #[test]
    fn config_set_and_bind_in_both_spellings() {
        let got = serve(&[
            "--config",
            "/tmp/a.toml",
            "--set",
            "recorder.memory_mb=64",
            "-s",
            "auth.token=a=b",
            "--bind=0.0.0.0:8080",
            "--set=backends.fake=true",
        ]);
        assert_eq!(got.config, Some(PathBuf::from("/tmp/a.toml")));
        assert_eq!(
            got.overrides,
            vec![
                ("recorder.memory_mb".to_string(), "64".to_string()),
                ("auth.token".to_string(), "a=b".to_string()),
                ("server.bind".to_string(), "0.0.0.0:8080".to_string()),
                ("backends.fake".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn help_wins_and_mistakes_are_errors() {
        assert_eq!(parse(&args(&["--bind", "x", "-h"])), Ok(Command::Help));
        assert!(parse(&args(&["--frobnicate"])).is_err());
        assert!(parse(&args(&["--set"])).is_err());
        assert!(parse(&args(&["--set", "no-equals"])).is_err());
        assert!(parse(&args(&["--set", "=value"])).is_err());
        assert!(parse(&args(&["--config"])).is_err());
    }

    #[test]
    fn config_path_prefers_the_flag_then_the_env_then_the_default() {
        let flag = serve(&["-c", "/a.toml"]);
        assert_eq!(
            config_path(&flag, Some("/b.toml".into())),
            (PathBuf::from("/a.toml"), true)
        );
        let none = ServeArgs::default();
        assert_eq!(
            config_path(&none, Some("/b.toml".into())),
            (PathBuf::from("/b.toml"), true)
        );
        assert_eq!(
            config_path(&none, None),
            (PathBuf::from(DEFAULT_CONFIG_PATH), false)
        );
    }
}

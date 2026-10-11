//! The only two guardrails in the system (SPECIFICATION.md §4.4, §7.1), and
//! the reason both are core functions rather than per-transport checks: the
//! web UI, MCP, and SCPI transports all end up calling into the same
//! `Registry`/`Instrument` operations, which call these — so there is no
//! separate check to forget when a new transport is added.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::PsuLimit;
use crate::error::CoreError;

/// The user's explicit USB port lock/unlock choices (SPECIFICATION.md §4.4):
/// session state, persisted under `[server] state_dir`, never configuration.
/// A port absent from the map follows the protection derived from what is
/// attached to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortOverrides {
    /// `"<hub>:<port>"` -> `true` locked by the user, `false` unlocked.
    ports: BTreeMap<String, bool>,
    /// `"<hub>:<port>"` -> what the port feeds (live test F-08). Absent in
    /// files written before labels existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<String, PortLabel>,
}

/// A user's name for what a port feeds, and whether it is the device under
/// test — so whoever switches the port knows what they are resetting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortLabel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub dut: bool,
}

/// The session file inside `state_dir`.
pub const PORT_OVERRIDES_FILE: &str = "usb-ports.json";

impl PortOverrides {
    fn key(hub: &str, port: u8) -> String {
        format!("{hub}:{port}")
    }

    /// The user's choice for a port, if they made one.
    pub fn get(&self, hub: &str, port: u8) -> Option<bool> {
        self.ports.get(&Self::key(hub, port)).copied()
    }

    /// Records a choice; `None` returns the port to its derived protection.
    pub fn set(&mut self, hub: &str, port: u8, protected: Option<bool>) {
        match protected {
            Some(p) => {
                self.ports.insert(Self::key(hub, port), p);
            }
            None => {
                self.ports.remove(&Self::key(hub, port));
            }
        }
    }

    /// The port's label and DUT mark, if it has either.
    pub fn label(&self, hub: &str, port: u8) -> Option<&PortLabel> {
        self.labels.get(&Self::key(hub, port))
    }

    /// Sets the port's name (`None` clears it) and, when given, its DUT mark;
    /// returns what is now on record. A port with neither is forgotten.
    pub fn set_label(
        &mut self,
        hub: &str,
        port: u8,
        label: Option<String>,
        dut: Option<bool>,
    ) -> PortLabel {
        let key = Self::key(hub, port);
        let mut entry = self.labels.remove(&key).unwrap_or_default();
        entry.label = label.filter(|l| !l.trim().is_empty());
        if let Some(dut) = dut {
            entry.dut = dut;
        }
        if entry.label.is_some() || entry.dut {
            self.labels.insert(key, entry.clone());
        }
        entry
    }

    /// Loads the session file. A missing file is a fresh session; a corrupt
    /// one is logged and treated as empty — every port then falls back to its
    /// derived protection, the safe direction.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!(path = %path.display(), %err, "unreadable USB port state; using defaults");
                Self::default()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "cannot read USB port state; using defaults");
                Self::default()
            }
        }
    }

    /// Writes the session file atomically (temp file + rename), so a crash
    /// mid-write never leaves a half-written file behind.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, path)
    }
}

/// The protection in force for a port: the user's choice if they made one,
/// otherwise the protection derived from what is attached (`derived` is its
/// reason, `None` when nothing protects it). Returns the reason when
/// protected.
pub fn port_protection(derived: Option<&str>, user: Option<bool>) -> Option<String> {
    match user {
        Some(true) => Some("locked by the user".to_string()),
        Some(false) => None,
        None => derived.map(str::to_string),
    }
}

/// SPECIFICATION.md §4.4: refuses to switch a protected port. The one check
/// every transport passes through; callers evaluate `derived` against a
/// fresh read of what is attached at the moment of switching.
pub fn check_port_unprotected(
    hub: &str,
    port: u8,
    derived: Option<&str>,
    user: Option<bool>,
) -> Result<(), CoreError> {
    match port_protection(derived, user) {
        Some(reason) => Err(CoreError::PortProtected {
            hub: hub.to_string(),
            port,
            reason,
        }),
        None => Ok(()),
    }
}

/// SPECIFICATION.md §7.1: refuses a PSU setpoint above the configured
/// `v_max`/`i_max` for `device`. An entry for the exact device id takes
/// precedence over a `"*"` wildcard entry. If no limit at all is configured
/// for the device (neither exact nor wildcard), the setpoint is allowed —
/// there is nothing to enforce against, which is a *configuration* gap, not
/// a runtime refusal.
pub fn check_soft_limits(
    limits: &[PsuLimit],
    device: &str,
    v_set: Option<f32>,
    i_set: Option<f32>,
) -> Result<(), CoreError> {
    let limit = limits
        .iter()
        .find(|l| l.device == device)
        .or_else(|| limits.iter().find(|l| l.device == "*"));

    let Some(limit) = limit else {
        return Ok(());
    };

    if let Some(v) = v_set {
        if v > limit.v_max {
            return Err(CoreError::SoftLimitExceeded {
                device: device.to_string(),
                field: "voltage".to_string(),
                value: v,
                limit: limit.v_max,
            });
        }
    }

    if let Some(i) = i_set {
        if i > limit.i_max {
            return Err(CoreError::SoftLimitExceeded {
                device: device.to_string(),
                field: "current".to_string(),
                value: i,
                limit: limit.i_max,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Vec<PsuLimit> {
        vec![PsuLimit {
            device: "*".into(),
            v_max: 5.5,
            i_max: 1.0,
        }]
    }

    #[test]
    fn a_port_follows_its_derived_protection_until_the_user_chooses() {
        let derived = Some("attached device is a hub");
        assert_eq!(port_protection(derived, None).as_deref(), derived);
        assert_eq!(port_protection(None, None), None);
    }

    #[test]
    fn the_users_choice_overrides_the_derived_rule_both_ways() {
        // Deliberately allow cutting a hub; deliberately protect a programmer.
        assert_eq!(
            port_protection(Some("attached device is a hub"), Some(false)),
            None
        );
        assert_eq!(
            port_protection(None, Some(true)).as_deref(),
            Some("locked by the user")
        );
    }

    #[test]
    fn a_protected_port_is_refused_with_its_reason() {
        let err =
            check_port_unprotected("3-3", 4, Some("attached device is a hub"), None).unwrap_err();
        match err {
            CoreError::PortProtected { hub, port, reason } => {
                assert_eq!((hub.as_str(), port), ("3-3", 4));
                assert!(reason.contains("hub"), "{reason}");
            }
            other => panic!("expected PortProtected, got {other:?}"),
        }
        assert!(check_port_unprotected("3-3", 3, None, None).is_ok());
    }

    #[test]
    fn overrides_survive_a_restart_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(PORT_OVERRIDES_FILE);
        let mut ov = PortOverrides::default();
        ov.set("3-3", 4, Some(false));
        ov.set("3-3.4", 1, Some(true));
        ov.save(&path).unwrap();

        let mut back = PortOverrides::load(&path);
        assert_eq!(back, ov);
        assert_eq!(back.get("3-3", 4), Some(false));
        back.set("3-3", 4, None);
        assert_eq!(back.get("3-3", 4), None, "cleared back to the derived rule");
    }

    #[test]
    fn labels_persist_and_older_files_without_them_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PORT_OVERRIDES_FILE);
        let mut ov = PortOverrides::default();
        let set = ov.set_label("3-3.4", 4, Some("RP2350 board".into()), Some(true));
        assert_eq!(set.label.as_deref(), Some("RP2350 board"));
        ov.save(&path).unwrap();
        let back = PortOverrides::load(&path);
        assert_eq!(back.label("3-3.4", 4), ov.label("3-3.4", 4));
        assert!(back.label("3-3.4", 4).unwrap().dut);

        // `dut` omitted keeps the mark; clearing both forgets the port.
        let mut ov = back;
        assert!(ov.set_label("3-3.4", 4, None, None).dut);
        assert!(!ov.set_label("3-3.4", 4, None, Some(false)).dut);
        assert_eq!(ov.label("3-3.4", 4), None);

        let old = dir.path().join("old.json");
        std::fs::write(&old, r#"{"ports":{"3-3:4":false}}"#).unwrap();
        assert_eq!(PortOverrides::load(&old).get("3-3", 4), Some(false));
    }

    #[test]
    fn a_missing_or_corrupt_state_file_falls_back_to_derived_protection() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            PortOverrides::load(&dir.path().join("absent.json")),
            PortOverrides::default()
        );
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert_eq!(PortOverrides::load(&bad), PortOverrides::default());
    }

    #[test]
    fn allows_a_setpoint_within_the_wildcard_limit() {
        assert!(check_soft_limits(&limits(), "3-3.4.2", Some(5.0), Some(0.5)).is_ok());
    }

    #[test]
    fn rejects_a_voltage_setpoint_above_v_max() {
        let err = check_soft_limits(&limits(), "3-3.4.2", Some(6.0), None).unwrap_err();
        assert_eq!(
            err,
            CoreError::SoftLimitExceeded {
                device: "3-3.4.2".into(),
                field: "voltage".into(),
                value: 6.0,
                limit: 5.5,
            }
        );
    }

    #[test]
    fn rejects_a_current_setpoint_above_i_max() {
        let err = check_soft_limits(&limits(), "3-3.4.2", None, Some(2.0)).unwrap_err();
        assert_eq!(
            err,
            CoreError::SoftLimitExceeded {
                device: "3-3.4.2".into(),
                field: "current".into(),
                value: 2.0,
                limit: 1.0,
            }
        );
    }

    #[test]
    fn a_setpoint_exactly_at_the_limit_is_allowed() {
        assert!(check_soft_limits(&limits(), "3-3.4.2", Some(5.5), Some(1.0)).is_ok());
    }

    #[test]
    fn device_specific_limit_takes_precedence_over_wildcard() {
        let mut all_limits = limits();
        all_limits.push(PsuLimit {
            device: "3-3.4.2".into(),
            v_max: 3.3,
            i_max: 0.5,
        });
        let err = check_soft_limits(&all_limits, "3-3.4.2", Some(4.0), None).unwrap_err();
        assert!(err.to_string().contains("3.3"));
    }

    #[test]
    fn no_configured_limit_allows_any_setpoint() {
        assert!(check_soft_limits(&[], "3-3.4.2", Some(100.0), Some(50.0)).is_ok());
    }
}

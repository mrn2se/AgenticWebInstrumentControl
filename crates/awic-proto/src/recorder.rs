//! `rec.*` wire types (SPECIFICATION.md §2.8, §2.9.3, §3.4).
//!
//! Kept in its own module so that `control.rs` stays small: only
//! the enum variants that dispatch on `op`/`event` live there — every
//! struct/enum a variant's fields reference is defined here instead.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::control::{AcqConfig, ChannelCalibration};

/// What a recording holds — the wire form of `awic-record::RecordingKind`
/// (SPEC §2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum RecKind {
    Digital,
    Analog,
}

/// Lifecycle of a recording (SPEC §3.4's `rec.state` event and `rec.list`):
/// still ingesting, ended cleanly (`rec.stop` or the device finished), or
/// ended badly (device loss, driver error). The data stays queryable in
/// every state until `rec.delete` or eviction (SPEC §2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "types.ts")]
pub enum RecordingState {
    Recording,
    Stopped,
    Error,
}

/// One channel of a recording, named the way the UI and `awic-pattern` name
/// it (`awic_pattern::ChannelInfo::name`: `"D0"`..`"D7"`, `"CH1"`, `"CH2"`),
/// with the calibration to convert its raw codes to physical units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecChannel {
    /// Hardware channel number (0-based), as in `AcqConfig.channels`.
    pub channel: u8,
    pub name: String,
    pub calibration: ChannelCalibration,
}

/// One entry of `rec.list` (SPEC §3.4, §2.8.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecordingSummary {
    pub rec: u16,
    pub device: String,
    pub kind: RecKind,
    pub rate_hz: u64,
    pub channels: Vec<RecChannel>,
    pub first_sample: u64,
    /// Exclusive end of the retained span — the sample index that would be
    /// appended next (`awic_record::Recording::head_sample`).
    pub head_sample: u64,
    pub bytes: u64,
    /// Compression ratio against packed/raw size (SPEC §2.8.4):
    /// `raw_bytes / bytes`. `1.0` for an empty recording.
    pub ratio: f64,
    pub state: RecordingState,
    /// Set when `state` is `error` (SPEC §3.4's `rec.state` event carries
    /// the same field; a client polling `rec.list` after missing that event
    /// still finds out why).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub msg: Option<String>,
    /// Spans with no data: the device stalled and was reset, and the
    /// recording resumed after it (B-25). Each span holds the last level
    /// before it, repeated, so later sample indices still match the time.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub gaps: Vec<RecGap>,
}

/// A span of a recording with no data in it (`RecordingSummary::gaps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecGap {
    /// First sample of the gap.
    pub start_sample: u64,
    /// Exclusive end: the first sample recorded after the resume.
    pub end_sample: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecStartResult {
    pub rec: u16,
    /// The effective configuration, exactly as `acq.configure` reports it
    /// (SPEC §2.3.1, §3.4) — `continuous` is always `true` here regardless
    /// of what the request carried (SPEC §2.8: a recording forces it on).
    pub cfg: AcqConfig,
    pub calibration: Vec<ChannelCalibration>,
}

/// The shared empty-ack shape for `rec.stop`/`rec.delete`/`rec.view.update`/
/// `rec.view.close`: nothing to report beyond success. `deny_unknown_fields`
/// for the same reason as `awic_proto::AcqStopResult` — without it this type
/// would swallow any later [`crate::ResponseResult`] variant's JSON in the
/// untagged enum. The four ops sharing one type is harmless: a client
/// already knows which op it called from the request `id`, so it never
/// needs the *type* to disambiguate an empty payload from another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export, export_to = "types.ts")]
pub struct RecAck {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecListResult {
    pub recordings: Vec<RecordingSummary>,
    /// `[recorder] memory_mb`, so a client can show "X / Y MB used" without
    /// a second request.
    pub budget_mb: u64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecViewResult {
    pub view: u16,
}

/// One pattern match (SPEC §2.9.2 rule 4, §2.9.3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct PatternMatch {
    pub t_s: f64,
    pub sample: u64,
    /// `rec.search` only: the matched run was already under way when the
    /// searched window opened and was cut by it, so `t_s` is the window
    /// start, not the event's true onset (B-44). Absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
}

/// The range actually searched (SPEC §2.9.3): echoes the request's
/// `t0_s`/`t1_s`, resolved against the retained span when either was
/// omitted, so a client that asked for the whole recording learns what that
/// meant in absolute time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct SearchedRange {
    pub t0_s: f64,
    pub t1_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecSearchResult {
    pub matches: Vec<PatternMatch>,
    /// `true` if `max_matches` was reached before `searched` was exhausted.
    pub truncated: bool,
    pub searched: SearchedRange,
    /// Sub-half-cell runs absorbed as glitches (SPEC §2.9.2 rule 2); 0 for
    /// every pattern kind except a bit sequence.
    pub glitches: u64,
    /// Set when the recording is no longer live: when it ended and why, so
    /// a result that stops short of "now" says so (live test B-24).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

/// `rec.wait`'s outcome (SPEC §2.9.3): untagged so the wire shape is exactly
/// `{ "match": {...} }` or `{ "timeout": true }`, the same convention as
/// [`crate::MeasurementResult`] — never a placeholder value standing in for
/// "it didn't happen."
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
#[ts(export, export_to = "types.ts")]
pub enum RecWaitResult {
    Match {
        #[serde(rename = "match")]
        r#match: PatternMatch,
    },
    Timeout {
        timeout: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "types.ts")]
pub struct RecSnapshotResult {
    /// `capture://<uuid>` handle: the range decompressed into an ordinary
    /// capture, so `capture.measure`, decoders and export work on it with
    /// no special case (SPEC §2.8.5).
    pub capture: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, Request, Response, ResponseResult};
    use serde_json::json;

    fn cfg() -> AcqConfig {
        AcqConfig {
            rate_hz: 1_000_000,
            channels: vec![0, 1, 2],
            samples: 0,
            trigger: None,
            ranges_v: vec![],
            continuous: true,
        }
    }

    #[test]
    fn rec_start_request_matches_spec_example() {
        let json = json!({
            "v": 1, "id": 1, "op": "rec.start", "device": "3-3.4.1.1",
            "cfg": { "rate_hz": 1000000, "channels": [0, 1, 2], "samples": 0,
                     "continuous": true }
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::RecStart {
                v: 1,
                id: 1,
                device: "3-3.4.1.1".into(),
                cfg: cfg(),
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
        assert_eq!(req.id(), 1);
    }

    #[test]
    fn rec_start_result_matches_spec_shape() {
        let result = ResponseResult::RecStart(RecStartResult {
            rec: 1,
            cfg: cfg(),
            calibration: vec![ChannelCalibration::digital(0)],
        });
        let resp = Response::ok(1, result);
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["result"]["rec"], 1);
        assert_eq!(value["result"]["cfg"]["continuous"], true);
        let back: Response = serde_json::from_value(value).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn rec_list_result_round_trips_with_a_channel() {
        let result = ResponseResult::RecList(RecListResult {
            recordings: vec![RecordingSummary {
                rec: 1,
                device: "3-3.4.1.1".into(),
                kind: RecKind::Digital,
                rate_hz: 1_000_000,
                channels: vec![RecChannel {
                    channel: 0,
                    name: "D0".into(),
                    calibration: ChannelCalibration::digital(0),
                }],
                first_sample: 0,
                head_sample: 41_002_100,
                bytes: 181_520,
                ratio: 225.9,
                state: RecordingState::Recording,
                msg: None,
                gaps: vec![],
            }],
            budget_mb: 256,
            used_bytes: 181_520,
        });
        let resp = Response::ok(1, result.clone());
        let value = serde_json::to_value(&resp).unwrap();
        let back: Response = serde_json::from_value(value).unwrap();
        assert_eq!(back, resp);
        assert_eq!(
            serde_json::to_value(&result).unwrap()["recordings"][0]["state"],
            "recording"
        );
    }

    #[test]
    fn rec_view_request_supports_follow_and_fixed_forms() {
        let follow = json!({
            "v": 1, "id": 4, "op": "rec.view", "rec": 1, "follow_s": 0.02,
            "columns": 1600, "fps": 30
        });
        let req: Request = serde_json::from_value(follow).unwrap();
        assert_eq!(
            req,
            Request::RecView {
                v: 1,
                id: 4,
                rec: 1,
                follow_s: Some(0.02),
                t0_s: None,
                t1_s: None,
                columns: 1600,
                fps: Some(30),
            }
        );

        let fixed = json!({
            "v": 1, "id": 5, "op": "rec.view", "rec": 1, "t0_s": 3.1, "t1_s": 3.3,
            "columns": 1600
        });
        let req: Request = serde_json::from_value(fixed).unwrap();
        assert_eq!(
            req,
            Request::RecView {
                v: 1,
                id: 5,
                rec: 1,
                follow_s: None,
                t0_s: Some(3.1),
                t1_s: Some(3.3),
                columns: 1600,
                fps: None,
            }
        );
    }

    #[test]
    fn rec_search_matches_spec_example() {
        let json = json!({
            "v": 1, "id": 1, "op": "rec.search", "rec": 1,
            "pattern": "D0 f=1k 100010x101", "t0_s": 0.0, "t1_s": 12.5, "max_matches": 16
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::RecSearch {
                v: 1,
                id: 1,
                rec: 1,
                pattern: "D0 f=1k 100010x101".into(),
                t0_s: Some(0.0),
                t1_s: Some(12.5),
                max_matches: Some(16),
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);

        let result = RecSearchResult {
            matches: vec![PatternMatch {
                t_s: 3.2140,
                sample: 3_214_000,
                truncated: false,
            }],
            truncated: false,
            searched: SearchedRange {
                t0_s: 0.0,
                t1_s: 12.5,
            },
            glitches: 0,
            note: None,
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "matches": [{ "t_s": 3.2140, "sample": 3214000 }],
                "truncated": false,
                "searched": { "t0_s": 0.0, "t1_s": 12.5 },
                "glitches": 0
            })
        );
    }

    #[test]
    fn rec_wait_request_matches_spec_example() {
        let json = json!({
            "v": 1, "id": 1, "op": "rec.wait", "rec": 1, "pattern": "D2 rising",
            "timeout_ms": 10000
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::RecWait {
                v: 1,
                id: 1,
                rec: 1,
                pattern: "D2 rising".into(),
                timeout_ms: Some(10_000),
                from_s: None,
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);
    }

    #[test]
    fn rec_wait_result_serializes_as_match_or_timeout_untagged() {
        let matched = RecWaitResult::Match {
            r#match: PatternMatch {
                t_s: 41.0021,
                sample: 41_002_100,
                truncated: false,
            },
        };
        assert_eq!(
            serde_json::to_value(matched).unwrap(),
            json!({ "match": { "t_s": 41.0021, "sample": 41002100 } })
        );
        let timed_out = RecWaitResult::Timeout { timeout: true };
        assert_eq!(
            serde_json::to_value(timed_out).unwrap(),
            json!({ "timeout": true })
        );
        for result in [matched, timed_out] {
            let value = serde_json::to_value(result).unwrap();
            let back: RecWaitResult = serde_json::from_value(value).unwrap();
            assert_eq!(back, result);
        }
    }

    #[test]
    fn rec_state_event_matches_spec_shape() {
        let event = Event::RecState {
            v: 1,
            rec: 1,
            state: RecordingState::Stopped,
            msg: Some("stopped by request".into()),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event"], "rec.state");
        assert_eq!(value["state"], "stopped");
        assert!(value.get("id").is_none());
        let back: Event = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn rec_start_forces_continuous_is_not_enforced_by_the_wire_type() {
        // The wire type carries whatever `continuous` the request sets; it
        // is the server (crate::server::recorder) that forces it to `true`
        // before calling `Instrument::configure` (SPEC §3.4: "rec.start is
        // acq.configure plus a continuous start in one step"). This test
        // just documents that the type itself round-trips `false` too, so a
        // future reader doesn't assume the proto layer enforces it.
        let json = json!({
            "v": 1, "id": 1, "op": "rec.start", "device": "1-1",
            "cfg": { "rate_hz": 1000, "channels": [0], "samples": 0 }
        });
        let req: Request = serde_json::from_value(json).unwrap();
        match req {
            Request::RecStart { cfg, .. } => assert!(!cfg.continuous),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rec_snapshot_request_and_result_round_trip() {
        let json = json!({
            "v": 1, "id": 1, "op": "rec.snapshot", "rec": 1, "t0_s": 3.0, "t1_s": 3.01
        });
        let req: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            req,
            Request::RecSnapshot {
                v: 1,
                id: 1,
                rec: 1,
                t0_s: 3.0,
                t1_s: 3.01,
            }
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), json);

        let result = RecSnapshotResult {
            capture: "capture://00000000-0000-0000-0000-000000000000".into(),
        };
        let back: RecSnapshotResult =
            serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
        assert_eq!(back, result);
    }
}

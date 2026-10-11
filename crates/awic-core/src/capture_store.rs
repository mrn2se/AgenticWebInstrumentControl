//! The in-memory capture ring (SPECIFICATION.md §2.7).
//!
//! - Bounded by **both** a capture count and a total-byte budget, whichever
//!   binds first; defaults 32 captures / 512 MB.
//! - In-memory only — a restart loses everything; `export_capture` (later
//!   task) is the only way anything survives one (Q-16).
//! - Analog is stored as raw device samples plus scale/offset, never
//!   pre-converted floats, so measurements (§5.4) run on what the hardware
//!   actually produced.
//! - Digital is stored as packed bits plus a transition list computed once
//!   at insert time and reused by both decoders (§6) and VCD export (§10).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use uuid::Uuid;

use crate::decimate::{bitwise_decimate, minmax_decimate};
use crate::device_id::DeviceId;
use crate::error::CoreError;
use awic_proto::frame::Encoding;

/// Default capture count bound (SPECIFICATION.md §2.7).
pub const DEFAULT_MAX_CAPTURES: usize = 32;
/// Default total-byte bound, in bytes (512 MB, SPECIFICATION.md §2.7).
pub const DEFAULT_MAX_BYTES: usize = 512 * 1024 * 1024;

/// Addresses a capture as `capture://<uuid>` (SPECIFICATION.md §2.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaptureId(pub Uuid);

impl CaptureId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CaptureId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CaptureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "capture://{}", self.0)
    }
}

/// Error returned by [`CaptureId::from_str`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureIdParseError {
    #[error("missing 'capture://' prefix in {0:?}")]
    MissingScheme(String),
    #[error("invalid uuid in {0:?}: {1}")]
    InvalidUuid(String, String),
}

impl FromStr for CaptureId {
    type Err = CaptureIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid_str = s
            .strip_prefix("capture://")
            .ok_or_else(|| CaptureIdParseError::MissingScheme(s.to_string()))?;
        let uuid = Uuid::parse_str(uuid_str)
            .map_err(|err| CaptureIdParseError::InvalidUuid(s.to_string(), err.to_string()))?;
        Ok(CaptureId(uuid))
    }
}

/// Metadata recorded alongside every capture's samples.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureMeta {
    pub device: DeviceId,
    pub rate_hz: u64,
    pub channel_count: u8,
    /// The hardware channel numbers this capture holds, in payload order:
    /// analog samples interleave exactly these channels, in this order; for
    /// digital, these are the bits that were actually acquired.
    ///
    /// Without it a capture only knows *positions*, and a client addressing
    /// hardware channel 1 of an acquisition of channels [1, 2] would silently
    /// get channel 2's data — a plausible, wrong measurement.
    pub channels: Vec<u8>,
    pub encoding: Encoding,
    /// Capture-relative t=0, in nanoseconds (matches the wire frame header).
    pub t0_ns: u64,
    /// Sample index of the trigger point, if the acquisition was triggered.
    pub trigger_position: Option<u64>,
}

/// One entry of a digital transition list: the packed-channel byte changed
/// to `channel_mask` at `sample_index`. Built once at insert time
/// ([`derive_transitions`]) and shared by decoders and VCD export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub sample_index: u64,
    pub channel_mask: u8,
}

/// The sample payload of a capture, kept in the form the hardware produced
/// it (SPECIFICATION.md §2.7) rather than pre-converted to physical units.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureData {
    /// `raw` is little-endian `u8` samples if `sample_bits <= 8`, else `i16`,
    /// channels interleaved. Volts for channel `c`, raw sample `s`:
    /// `s as f32 * channel_scale[c] + channel_offset[c]`.
    Analog {
        raw: Vec<u8>,
        sample_bits: u8,
        channel_scale: Vec<f32>,
        channel_offset: Vec<f32>,
    },
    /// One byte per sample, one bit per channel.
    Digital {
        packed: Vec<u8>,
        transitions: Vec<Transition>,
    },
}

impl CaptureData {
    fn byte_len(&self) -> usize {
        match self {
            CaptureData::Analog { raw, .. } => raw.len(),
            CaptureData::Digital {
                packed,
                transitions,
            } => packed.len() + transitions.len() * std::mem::size_of::<Transition>(),
        }
    }
}

/// Arguments for [`CaptureStore::insert_analog`], grouped into a struct so
/// the constructor doesn't take ten positional parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAnalogCapture {
    /// Hardware channel numbers in payload order — see [`CaptureMeta::channels`].
    pub channels: Vec<u8>,
    /// The id `Instrument::start` returned for this acquisition. The store
    /// files the capture under it rather than minting its own — see
    /// [`CaptureStore::insert_with_id`].
    pub id: CaptureId,
    pub device: DeviceId,
    pub rate_hz: u64,
    pub channel_count: u8,
    pub sample_bits: u8,
    pub channel_scale: Vec<f32>,
    pub channel_offset: Vec<f32>,
    pub raw: Vec<u8>,
    pub t0_ns: u64,
    pub trigger_position: Option<u64>,
}

/// Arguments for [`CaptureStore::insert_digital`] — the digital counterpart
/// of [`NewAnalogCapture`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewDigitalCapture {
    /// Hardware channel numbers in payload order — see [`CaptureMeta::channels`].
    pub channels: Vec<u8>,
    /// The id `Instrument::start` returned for this acquisition.
    pub id: CaptureId,
    pub device: DeviceId,
    pub rate_hz: u64,
    pub channel_count: u8,
    pub packed: Vec<u8>,
    pub t0_ns: u64,
    pub trigger_position: Option<u64>,
}

/// One stored capture: metadata plus samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub id: CaptureId,
    pub meta: CaptureMeta,
    pub data: CaptureData,
}

/// Builds the transition list for a packed-digital sample buffer: one entry
/// each time the packed byte changes value, recording the sample index and
/// the new channel state.
pub fn derive_transitions(packed: &[u8]) -> Vec<Transition> {
    let mut transitions = Vec::new();
    let mut prev: Option<u8> = None;
    for (i, &sample) in packed.iter().enumerate() {
        if prev != Some(sample) {
            transitions.push(Transition {
                sample_index: i as u64,
                channel_mask: sample,
            });
            prev = Some(sample);
        }
    }
    transitions
}

/// A capture's samples, decimated to at most `max_points` min/max pairs
/// (SPECIFICATION.md §8.3's `read_capture`), in the same representation the
/// data was stored in.
#[derive(Debug, Clone, PartialEq)]
pub enum DecimatedSamples {
    U8(Vec<(u8, u8)>),
    I16(Vec<(i16, i16)>),
}

/// How many evicted ids the store remembers. An id is 16 bytes plus a few
/// words, so a few hundred cost nothing; the bound exists only so a
/// long-running server cannot grow without limit (B-37).
pub const MAX_TOMBSTONES: usize = 256;

/// What the store remembers of an evicted capture: enough to tell the client
/// *that* it was evicted, when, and to make room for what (B-37).
#[derive(Debug, Clone)]
struct Tombstone {
    id: CaptureId,
    at: Instant,
    /// The capture whose arrival forced this one out.
    for_capture: CaptureId,
}

struct Inner {
    /// Oldest first.
    order: VecDeque<CaptureId>,
    /// Evicted ids, oldest first, at most [`MAX_TOMBSTONES`] (B-37).
    tombstones: VecDeque<Tombstone>,
    total_bytes: usize,
    captures: HashMap<CaptureId, Arc<Capture>>,
}

/// The capture ring itself. Cheap to clone-share: wrap in `Arc` at the call
/// site if needed, same as [`crate::EventBus`].
pub struct CaptureStore {
    inner: RwLock<Inner>,
    max_count: usize,
    max_bytes: usize,
}

impl CaptureStore {
    pub fn new(max_count: usize, max_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(Inner {
                order: VecDeque::new(),
                tombstones: VecDeque::new(),
                total_bytes: 0,
                captures: HashMap::new(),
            }),
            max_count,
            max_bytes,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_CAPTURES, DEFAULT_MAX_BYTES)
    }

    /// Inserts a capture, then evicts the oldest entries — never the one
    /// just inserted — until both the count and byte bounds hold, or only
    /// one capture remains (a single capture over budget is kept: there is
    /// nothing older left to evict).
    pub fn insert(&self, meta: CaptureMeta, data: CaptureData) -> CaptureId {
        let id = CaptureId::new();
        self.insert_with_id(id, meta, data);
        id
    }

    /// Files a capture under a caller-supplied id.
    ///
    /// This is the path real acquisitions must use. `Instrument::start`
    /// mints the `CaptureId` and reports it to the client (`acq.start`'s
    /// result); if the store then minted a *second* id, the client would ask
    /// for capture X while the store only held capture Y. One acquisition,
    /// one id, end to end.
    ///
    /// Re-inserting an id that is already present replaces that capture and
    /// keeps the byte/count accounting exact — a duplicate cannot leave a
    /// ghost entry in the eviction order.
    pub fn insert_with_id(&self, id: CaptureId, meta: CaptureMeta, data: CaptureData) {
        let len = data.byte_len();
        let capture = Arc::new(Capture { id, meta, data });

        let mut inner = self.inner.write().expect("capture store lock poisoned");
        if let Some(previous) = inner.captures.remove(&id) {
            inner.total_bytes -= previous.data.byte_len();
            inner.order.retain(|existing| *existing != id);
        }
        inner.tombstones.retain(|t| t.id != id);
        inner.order.push_back(id);
        inner.captures.insert(id, capture);
        inner.total_bytes += len;

        while inner.order.len() > 1
            && (inner.order.len() > self.max_count || inner.total_bytes > self.max_bytes)
        {
            if let Some(evict_id) = inner.order.pop_front() {
                if let Some(evicted) = inner.captures.remove(&evict_id) {
                    inner.total_bytes -= evicted.data.byte_len();
                }
                inner.tombstones.push_back(Tombstone {
                    id: evict_id,
                    at: Instant::now(),
                    for_capture: id,
                });
                while inner.tombstones.len() > MAX_TOMBSTONES {
                    inner.tombstones.pop_front();
                }
            }
        }
    }

    /// The error for a capture handle the store cannot produce: a distinct
    /// `CaptureEvicted` when the id was evicted (and says when, for what,
    /// and the store's bounds), plain `CaptureNotFound` for an id the store
    /// never held — a typo must not read like a loss, nor a loss like a
    /// typo (B-37). `handle` is the text the client sent, echoed back.
    pub fn missing_error(&self, id: &CaptureId, handle: &str) -> CoreError {
        let inner = self.inner.read().expect("capture store lock poisoned");
        match inner.tombstones.iter().find(|t| t.id == *id) {
            Some(t) => CoreError::CaptureEvicted(format!(
                "{handle} was evicted {} s ago to make room for {} — the store keeps at most \
                 {} captures / {} MB and drops the oldest first. Re-run `capture` (or \
                 `snapshot_recording`) to get a new handle, and work on large captures \
                 promptly or read them in smaller ones",
                t.at.elapsed().as_secs(),
                t.for_capture,
                self.max_count,
                self.max_bytes / (1024 * 1024),
            )),
            None => CoreError::CaptureNotFound(handle.to_string()),
        }
    }

    /// The store's bounds as `(max captures, max bytes)`, for tool
    /// descriptions and messages that must state them (B-37).
    pub fn bounds(&self) -> (usize, usize) {
        (self.max_count, self.max_bytes)
    }

    pub fn insert_analog(&self, input: NewAnalogCapture) -> CaptureId {
        let encoding = if input.sample_bits <= 8 {
            Encoding::U8Interleaved
        } else {
            Encoding::I16
        };
        let meta = CaptureMeta {
            device: input.device,
            rate_hz: input.rate_hz,
            channel_count: input.channel_count,
            channels: input.channels,
            encoding,
            t0_ns: input.t0_ns,
            trigger_position: input.trigger_position,
        };
        let data = CaptureData::Analog {
            raw: input.raw,
            sample_bits: input.sample_bits,
            channel_scale: input.channel_scale,
            channel_offset: input.channel_offset,
        };
        self.insert_with_id(input.id, meta, data);
        input.id
    }

    pub fn insert_digital(&self, input: NewDigitalCapture) -> CaptureId {
        let transitions = derive_transitions(&input.packed);
        let meta = CaptureMeta {
            device: input.device,
            rate_hz: input.rate_hz,
            channel_count: input.channel_count,
            channels: input.channels,
            encoding: Encoding::PackedDigital,
            t0_ns: input.t0_ns,
            trigger_position: input.trigger_position,
        };
        let data = CaptureData::Digital {
            packed: input.packed,
            transitions,
        };
        self.insert_with_id(input.id, meta, data);
        input.id
    }

    pub fn get(&self, id: &CaptureId) -> Option<Arc<Capture>> {
        self.inner
            .read()
            .expect("capture store lock poisoned")
            .captures
            .get(id)
            .cloned()
    }

    pub fn remove(&self, id: &CaptureId) -> bool {
        let mut inner = self.inner.write().expect("capture store lock poisoned");
        if let Some(capture) = inner.captures.remove(id) {
            inner.total_bytes -= capture.data.byte_len();
            inner.order.retain(|existing| existing != id);
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("capture store lock poisoned")
            .order
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn total_bytes(&self) -> usize {
        self.inner
            .read()
            .expect("capture store lock poisoned")
            .total_bytes
    }

    /// Decimates a stored capture to at most `max_points` min/max pairs
    /// (SPECIFICATION.md §8.3). Returns `None` if `id` is not in the store.
    pub fn read_capture(&self, id: &CaptureId, max_points: usize) -> Option<DecimatedSamples> {
        let capture = self.get(id)?;
        Some(match &capture.data {
            CaptureData::Analog {
                raw, sample_bits, ..
            } if *sample_bits <= 8 => DecimatedSamples::U8(minmax_decimate(raw, max_points)),
            CaptureData::Analog { raw, .. } => {
                let samples: Vec<i16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes(*c))
                    .collect();
                DecimatedSamples::I16(minmax_decimate(&samples, max_points))
            }
            CaptureData::Digital { packed, .. } => {
                // Bitmasks, not numbers: AND/OR per bucket, never min/max.
                DecimatedSamples::U8(bitwise_decimate(packed, max_points))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> DeviceId {
        DeviceId::new(3, vec![3, 4, 2], 0x1d50, 0x608e)
    }

    fn analog_meta() -> CaptureMeta {
        CaptureMeta {
            channels: (0..1).collect(),
            device: dev(),
            rate_hz: 1_000,
            channel_count: 1,
            encoding: Encoding::U8Interleaved,
            t0_ns: 0,
            trigger_position: None,
        }
    }

    fn analog_of_size(bytes: usize) -> CaptureData {
        CaptureData::Analog {
            raw: vec![0u8; bytes],
            sample_bits: 8,
            channel_scale: vec![1.0],
            channel_offset: vec![0.0],
        }
    }

    #[test]
    fn evicts_oldest_once_count_bound_exceeded() {
        let store = CaptureStore::new(2, usize::MAX);
        let a = store.insert(analog_meta(), analog_of_size(1));
        let b = store.insert(analog_meta(), analog_of_size(1));
        let c = store.insert(analog_meta(), analog_of_size(1));

        assert_eq!(store.len(), 2);
        assert!(store.get(&a).is_none(), "oldest capture should be evicted");
        assert!(store.get(&b).is_some());
        assert!(store.get(&c).is_some());
    }

    #[test]
    fn an_evicted_id_is_reported_as_evicted_not_as_unknown() {
        // B-37: the same words for a typo and a loss sent a client round
        // in circles.
        let store = CaptureStore::new(1, usize::MAX);
        let a = store.insert(analog_meta(), analog_of_size(10));
        let b = store.insert(analog_meta(), analog_of_size(10));
        match store.missing_error(&a, &a.to_string()) {
            CoreError::CaptureEvicted(msg) => {
                assert!(
                    msg.contains(&b.to_string()),
                    "names the displacing capture: {msg}"
                );
                assert!(msg.contains("1 captures"), "states the bound: {msg}");
            }
            other => panic!("expected CaptureEvicted, got {other:?}"),
        }
        let never = CaptureId::new();
        assert!(matches!(
            store.missing_error(&never, "x"),
            CoreError::CaptureNotFound(_)
        ));
    }

    #[test]
    fn tombstones_are_bounded() {
        let store = CaptureStore::new(1, usize::MAX);
        let first = store.insert(analog_meta(), analog_of_size(1));
        for _ in 0..(MAX_TOMBSTONES + 5) {
            store.insert(analog_meta(), analog_of_size(1));
        }
        assert!(matches!(
            store.missing_error(&first, "x"),
            CoreError::CaptureNotFound(_)
        ));
        assert_eq!(store.inner.read().unwrap().tombstones.len(), MAX_TOMBSTONES);
    }

    #[test]
    fn evicts_oldest_once_byte_bound_exceeded() {
        let store = CaptureStore::new(usize::MAX, 25);
        let a = store.insert(analog_meta(), analog_of_size(10));
        let b = store.insert(analog_meta(), analog_of_size(10));
        // Total would be 30 > 25: `a` must go.
        let c = store.insert(analog_meta(), analog_of_size(10));

        assert!(store.get(&a).is_none());
        assert!(store.get(&b).is_some());
        assert!(store.get(&c).is_some());
        assert!(store.total_bytes() <= 25 || store.len() == 1);
    }

    #[test]
    fn a_single_oversized_capture_is_kept_even_over_byte_bound() {
        let store = CaptureStore::new(usize::MAX, 5);
        let a = store.insert(analog_meta(), analog_of_size(1000));
        assert!(store.get(&a).is_some());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn count_and_byte_bounds_both_apply_whichever_binds_first() {
        // Count bound is loose (10), byte bound is tight (15) — byte bound
        // must be the one that evicts.
        let store = CaptureStore::new(10, 15);
        for _ in 0..5 {
            store.insert(analog_meta(), analog_of_size(10));
        }
        assert!(store.len() < 5, "byte bound should have evicted entries");
        assert!(store.total_bytes() <= 15 || store.len() == 1);
    }

    #[test]
    fn capture_is_filed_under_the_acquisitions_id() {
        // The id Instrument::start returns must be the id the store uses,
        // or a client reading back the capture it was told about gets None.
        let store = CaptureStore::with_defaults();
        let acquisition_id = CaptureId::new();
        let returned = store.insert_digital(NewDigitalCapture {
            channels: (0..1).collect(),
            id: acquisition_id,
            device: dev(),
            rate_hz: 1_000,
            channel_count: 1,
            packed: vec![0, 1],
            t0_ns: 0,
            trigger_position: None,
        });
        assert_eq!(returned, acquisition_id);
        assert!(store.get(&acquisition_id).is_some());
    }

    #[test]
    fn reinserting_an_id_replaces_it_without_corrupting_accounting() {
        let store = CaptureStore::with_defaults();
        let id = CaptureId::new();
        store.insert_with_id(id, analog_meta(), analog_of_size(100));
        store.insert_with_id(id, analog_meta(), analog_of_size(40));
        assert_eq!(
            store.len(),
            1,
            "a duplicate id must not create a ghost entry"
        );
        assert_eq!(store.total_bytes(), analog_of_size(40).byte_len());
    }

    #[test]
    fn digital_insert_derives_transitions_once() {
        let store = CaptureStore::with_defaults();
        let packed = vec![0u8, 0, 1, 1, 1, 0, 2, 2];
        let id = store.insert_digital(NewDigitalCapture {
            channels: (0..3).collect(),
            id: CaptureId::new(),
            device: dev(),
            rate_hz: 1_000_000,
            channel_count: 3,
            packed,
            t0_ns: 0,
            trigger_position: None,
        });
        let capture = store.get(&id).unwrap();
        match &capture.data {
            CaptureData::Digital { transitions, .. } => {
                assert_eq!(
                    transitions,
                    &vec![
                        Transition {
                            sample_index: 0,
                            channel_mask: 0
                        },
                        Transition {
                            sample_index: 2,
                            channel_mask: 1
                        },
                        Transition {
                            sample_index: 5,
                            channel_mask: 0
                        },
                        Transition {
                            sample_index: 6,
                            channel_mask: 2
                        },
                    ]
                );
            }
            _ => panic!("expected digital capture"),
        }
    }

    #[test]
    fn read_capture_decimates_and_preserves_extremes() {
        let store = CaptureStore::with_defaults();
        let mut raw = vec![10u8; 100];
        raw[42] = 250;
        let id = store.insert_analog(NewAnalogCapture {
            channels: (0..1).collect(),
            id: CaptureId::new(),
            device: dev(),
            rate_hz: 1_000,
            channel_count: 1,
            sample_bits: 8,
            channel_scale: vec![1.0],
            channel_offset: vec![0.0],
            raw,
            t0_ns: 0,
            trigger_position: None,
        });

        let decimated = store.read_capture(&id, 10).unwrap();
        match decimated {
            DecimatedSamples::U8(pairs) => {
                assert!(pairs.len() <= 10);
                assert!(pairs.iter().any(|&(_, hi)| hi == 250));
            }
            _ => panic!("expected u8 decimation for 8-bit analog"),
        }
    }

    #[test]
    fn read_capture_returns_none_for_unknown_id() {
        let store = CaptureStore::with_defaults();
        assert!(store.read_capture(&CaptureId::new(), 10).is_none());
    }

    #[test]
    fn remove_updates_len_and_total_bytes() {
        let store = CaptureStore::with_defaults();
        let id = store.insert(analog_meta(), analog_of_size(10));
        assert_eq!(store.total_bytes(), 10);
        assert!(store.remove(&id));
        assert_eq!(store.len(), 0);
        assert_eq!(store.total_bytes(), 0);
        assert!(!store.remove(&id));
    }

    #[test]
    fn capture_id_display_matches_capture_uri_scheme() {
        let id = CaptureId::new();
        assert!(id.to_string().starts_with("capture://"));
    }

    #[test]
    fn capture_id_parses_its_own_display_output() {
        let id = CaptureId::new();
        let parsed: CaptureId = id.to_string().parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn capture_id_parse_rejects_missing_scheme_and_bad_uuid() {
        assert_eq!(
            "00000000-0000-0000-0000-000000000000".parse::<CaptureId>(),
            Err(CaptureIdParseError::MissingScheme(
                "00000000-0000-0000-0000-000000000000".to_string()
            ))
        );
        assert!(matches!(
            "capture://not-a-uuid".parse::<CaptureId>(),
            Err(CaptureIdParseError::InvalidUuid(_, _))
        ));
    }
}

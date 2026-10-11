//! The `fx2scope` [`Instrument`] — a 2-channel analog oscilloscope on a
//! Cypress FX2 (SPECIFICATION.md §2.5), reached at bus path `3-3.4.2` on this
//! bench: `04b4:602a` unprogrammed, `1d50:608e` after firmware (see
//! [`super::protocol`]'s doc comment for why that differs from
//! SPECIFICATION.md's assumed `04b5:602a`).
//!
//! Structure mirrors `fx2lafw::driver` deliberately — same bounded-completion
//! streaming loop, same stall/cancel race via `tokio::select!`, same rate
//! measurement — because the lessons that shaped it (SPECIFICATION.md §2.6,
//! the fx2lafw driver's hardware findings) apply unchanged here: a silent
//! device must never park `stop()` inside `next_complete()`, and USB 2.0
//! bulk bandwidth is a hard ceiling this driver must refuse to exceed rather
//! than discover by hanging.
//!
//! # Known gap: bulk streaming is unverified end-to-end with `nusb`
//!
//! Every control-plane fact in this driver (`0xE0`-`0xE3` register values,
//! the rate ladder, the range table, the calibration formula, the absence
//! of `0xE4`/on-device calibration) is verified against the real device —
//! see [`super::protocol`]'s doc comment — and `prepare()` (firmware upload,
//! re-enumeration by bus path) and `configure()` (rate/range validation,
//! honest effective config) are hardware-tested and pass.
//!
//! `start()`'s bulk data path is hardware-verified against the front-panel
//! CAL square wave (see `protocol::MAX_SUSTAINED_RATE_HZ` for the table). It
//! did not work as first written, and the four causes are worth knowing:
//!
//! 1. **Register values go in the control transfer's data stage**, one byte,
//!    with wValue 0 — found by capturing sigrok's and this driver's traffic
//!    with usbmon and comparing packet by packet. With the value in wValue
//!    the firmware configured nothing and never started; the only bytes that
//!    arrived were stale FIFO content, which looked like `nusb` failing.
//! 2. **Stale data is discarded** at every start (`STALE_FIFO_BYTES`).
//! 3. **Several transfers stay in flight** (`transfer_count`), so a read is
//!    always pending; one-at-a-time lost samples at every transfer boundary.
//! 4. **Reads are submitted before the start command**, closing a window in
//!    which the FIFO overran near the start of fast captures.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awic_core::{
    AcqConfig, Capability, CaptureId, ChannelCalibration, CoreError, DeviceId, EffectiveAcq,
    Instrument, Result as CoreResult, SampleChunk, SampleSink,
};
use awic_proto::frame::Encoding;
use bytes::Bytes;
use nusb::transfer::{Bulk, ControlOut, ControlType, In, Recipient};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::error::{NativeError, Result};
use crate::firmware::FirmwareResolver;
use crate::fx2;

use super::protocol::{self, Range, CHANNEL_COUNT};

/// The Debian `sigrok-firmware-fx2lafw` package's filename for this device —
/// confirmed on the bench (see [`super::protocol`]'s doc comment) by running
/// `sigrok-cli --driver hantek-6xxx -l 5 --scan` and reading its `ezusb:
/// Uploading firmware 'fx2lafw-hantek-6022bl.fw'` log line.
pub const FIRMWARE_FILE: &str = "fx2lafw-hantek-6022bl.fw";

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Bytes on the wire per timepoint: this device always sends both channels
/// interleaved regardless of the host's channel selection (SPECIFICATION.md
/// §2.5's verified ground truth — see [`protocol`]'s `0xE4` doc comment), so
/// bandwidth and buffer sizing must be computed from 2 bytes/sample even for
/// a 1-channel capture.
const WIRE_BYTES_PER_SAMPLE: u64 = 2;

/// See `fx2lafw::driver::MIN_STALL_TIMEOUT`'s doc comment for why a floor
/// exists at all: a hardware test hung for over four hours awaiting a
/// silent endpoint before that bound was added to that driver.
///
/// This device's own floor is much higher than `fx2lafw`'s 1 s. Bench
/// finding, traced at the `usbfs` `ioctl` level (`strace -e ioctl` on
/// `sigrok-cli`, since `usbmon` is unavailable under this host's kernel
/// lockdown — see [`super::protocol`]'s doc comment): the device delivers
/// data in **bursts with gaps up to ~650 ms between them**, independent of
/// the requested buffer size — a small buffer just means *more* such gaps
/// to wait through, not shorter ones. `libsigrok`'s own bulk reads use a
/// flat 4000 ms per-transfer timeout (`protocol.c`'s
/// `scope_6xxx_get_channeldata`, read for this fact), which this driver's
/// floor is set comfortably above.
const MIN_STALL_TIMEOUT: Duration = Duration::from_secs(6);
const STALL_BUFFERS: u32 = 10;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const RATE_CHECK_MIN_SPAN: Duration = Duration::from_millis(300);
/// Allowed deviation of the measured rate from the configured one.
///
/// Tighter than `fx2lafw`'s 10 %, and for a measured reason: past the bus
/// ceiling this device drops samples *evenly*, and at 24 MHz that is 6.5 %
/// (22.53 MS/s delivered) — which a 10 % tolerance lets through as a
/// plausible capture on a wrong timebase. Honest captures here measure
/// within 0.3 % (16 MHz: 16 002 xxx; 1 MHz: 1 000 1xx), because several
/// transfers in flight give many completions to time.
const RATE_TOLERANCE: f64 = 0.03;

fn stall_timeout(rate_hz: u64, buf_len: usize) -> Duration {
    let bytes_per_sec = rate_hz.max(1) * WIRE_BYTES_PER_SAMPLE;
    let fill = Duration::from_secs_f64(buf_len as f64 / bytes_per_sec as f64);
    (fill * STALL_BUFFERS).max(MIN_STALL_TIMEOUT)
}

fn measured_rate(first: Instant, last: Instant, bytes_after_first: u64) -> Option<f64> {
    let span = last.saturating_duration_since(first);
    (span >= RATE_CHECK_MIN_SPAN && bytes_after_first > 0)
        .then(|| (bytes_after_first as f64 / WIRE_BYTES_PER_SAMPLE as f64) / span.as_secs_f64())
}

fn rate_within_tolerance(measured: f64, configured: u64) -> bool {
    (measured / configured.max(1) as f64 - 1.0).abs() <= RATE_TOLERANCE
}

/// Bytes discarded at the start of every acquisition: samples taken *before*
/// this start, under the previous configuration. Measured on the bench, not
/// assumed:
///
/// - After a range change from ±5 V to ±0.5 V, the first ~1024 timepoints
///   sat at the old gain's levels: the FX2's EP6 FIFO, 4 x 512 B.
/// - Alternating ranges twelve times showed exactly **5 more** stale
///   timepoints (10 B) beyond the FIFO in *every* capture at 8 MHz, and
///   occasionally at 1 MHz — the device's internal data pipeline. They
///   produced a false edge a few samples into the capture, a short first
///   period and a skewed frequency.
///
/// So: the FIFO, plus 32 B (16 timepoints) — the measured 10 B with margin,
/// and even, keeping channel pairs aligned. sigrok flushes only 1024 B and
/// keeps more than half of the stale data.
const STALE_FIFO_BYTES: usize = 4 * 512 + 32;

/// Wire data per USB transfer, in milliseconds of sampling.
const TARGET_CHUNK_MS: u64 = 8;
/// Wire data kept in flight: the slack that absorbs a momentary stall in our
/// own task without the device's 2 KB FIFO overrunning.
const TARGET_INFLIGHT_MS: u64 = 250;
const MIN_TRANSFERS: usize = 4;
const MAX_TRANSFERS: usize = 32;
/// Cap on bytes in flight. usbfs allows 16 MB per process by default, shared
/// by every device this server drives.
const MAX_INFLIGHT_BYTES: usize = 8 * 1024 * 1024;

/// One transfer's size: `TARGET_CHUNK_MS` of wire data, a multiple of the
/// max packet size.
fn buffer_len(rate_hz: u64, max_packet_size: usize) -> usize {
    let mp = max_packet_size.max(1);
    let bytes_per_ms = (rate_hz * WIRE_BYTES_PER_SAMPLE / 1000).max(1) as usize;
    (TARGET_CHUNK_MS as usize * bytes_per_ms)
        .div_ceil(mp)
        .max(1)
        * mp
}

/// How many transfers to keep in flight so a read is always pending.
///
/// An earlier version kept exactly one outstanding transfer, sized for the
/// whole capture, on the belief that this device could not sustain several.
/// That belief was formed while the device was never actually started (its
/// register writes carried the value in the wrong place), so it rested on
/// nothing. With one transfer, every resubmit leaves no read pending, and at
/// 32 MB/s the FIFO overruns in ~64 us — silently dropping samples at every
/// transfer boundary of a long capture.
fn transfer_count(rate_hz: u64, buf_len: usize) -> usize {
    let bytes_per_ms = (rate_hz * WIRE_BYTES_PER_SAMPLE / 1000).max(1) as usize;
    let wanted = (TARGET_INFLIGHT_MS as usize * bytes_per_ms) / buf_len.max(1);
    let cap = (MAX_INFLIGHT_BYTES / buf_len.max(1)).max(MIN_TRANSFERS);
    wanted.clamp(MIN_TRANSFERS, MAX_TRANSFERS).min(cap)
}

/// Diagnostics from the most recently completed (or in-progress)
/// acquisition — see `fx2lafw::driver::AcquisitionStats`'s doc comment; this
/// is the same contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcquisitionStats {
    pub requested_samples: u64,
    pub received_samples: u64,
    pub dropped_transfers: usize,
    pub max_transfers_in_flight: usize,
    pub min_transfers_in_flight: usize,
    pub stalled: bool,
    pub measured_rate_hz: Option<f64>,
    pub rate_mismatch: bool,
    /// This acquisition ran until `stop()` rather than for a fixed sample
    /// count (SPECIFICATION.md §2.3.2) — `requested_samples` is then
    /// meaningless (`samples` is ignored) and callers must not compare
    /// `received_samples` against it.
    pub continuous: bool,
}

struct ReadyState {
    device: nusb::Device,
    interface: nusb::Interface,
    bulk_in_addr: u8,
    max_packet_size: usize,
}

enum State {
    Unprepared,
    Ready(ReadyState),
}

/// What `configure()` applied, kept so `start()` can re-derive the exact
/// commands and calibration without re-validating (`configure()` already
/// did that).
#[derive(Clone)]
struct Applied {
    cfg: AcqConfig,
    ranges: Vec<Range>, // one per entry of cfg.channels, in order.
}

pub struct Fx2ScopeInstrument {
    id: DeviceId,
    capabilities: Vec<Capability>,
    firmware_resolver: Arc<FirmwareResolver>,
    state: State,
    applied: Option<Applied>,
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    stats: Arc<Mutex<AcquisitionStats>>,
}

impl Fx2ScopeInstrument {
    pub fn new(id: DeviceId, firmware_resolver: Arc<FirmwareResolver>) -> Self {
        Self {
            id,
            capabilities: vec![Capability::Analog {
                channels: CHANNEL_COUNT,
                bits: 8,
                rates_hz: protocol::sustained_rates_hz(),
                ranges_v: protocol::RANGES.iter().map(|r| r.range_v).collect(),
            }],
            firmware_resolver,
            state: State::Unprepared,
            applied: None,
            cancel: None,
            task: None,
            stats: Arc::new(Mutex::new(AcquisitionStats::default())),
        }
    }

    pub fn last_stats(&self) -> AcquisitionStats {
        self.stats.lock().expect("stats mutex poisoned").clone()
    }

    fn ready(&self) -> Result<&ReadyState> {
        match &self.state {
            State::Ready(r) => Ok(r),
            State::Unprepared => Err(NativeError::NotPrepared("configure")),
        }
    }
}

/// See `fx2lafw::driver::drain`'s doc comment: bounded so an endpoint that
/// never returns cancelled transfers cannot hang the caller.
async fn drain(endpoint: &mut nusb::Endpoint<Bulk, In>) {
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while endpoint.pending() > 0 {
            let _ = endpoint.next_complete().await;
        }
    })
    .await;
    if drained.is_err() {
        tracing::error!(
            pending = endpoint.pending(),
            "fx2scope transfers did not drain"
        );
    }
}

/// Bench finding: vendor control writes issued in the first few hundred
/// milliseconds after a fresh `prepare()` (firmware upload + re-enumeration)
/// unpredictably time out (`TransferError::Cancelled` — nusb's variant
/// covers both cancellation and timeout), even though the identical write
/// succeeds immediately once the device has settled. Reproduced repeatedly
/// on the bench: `configure()` alone (no register writes) never fails after
/// `prepare()`, but the register writes `start()` issues right after a fresh
/// upload do — not consistently the *first* one, so a retry scoped to only
/// the first write was not enough; every write in the sequence needs the
/// same bounded retry. The device's USB stack answers enumeration requests
/// (it is fully enumerated by the time `wait_for_reenumeration` returns)
/// before its application firmware has finished initializing the
/// vendor-request handler for the *analog* command set — a readiness race,
/// not a stale value, unlike `fx2lafw`'s firmware-image quirk (a rate that
/// runs at the previous setting). A few bounded retries clear it every time
/// observed on this bench; kept small so a genuinely absent/wedged device
/// still fails within a bounded number of [`CONTROL_TIMEOUT`] periods rather
/// than masking a real fault.
const WRITE_REG_RETRIES: u32 = 5;
const WRITE_REG_RETRY_DELAY: Duration = Duration::from_millis(100);

async fn write_reg(device: &nusb::Device, request: u8, value: u8) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..WRITE_REG_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(WRITE_REG_RETRY_DELAY).await;
        }
        match device
            .control_out(
                // The value travels in the DATA STAGE (one byte), with
                // wValue = 0 — verified packet-by-packet against sigrok with
                // usbmon. This firmware reads the EP0 data buffer and ignores
                // wValue, so a value in wValue with no data stage configures
                // nothing: an earlier version did exactly that, and the
                // device never started — its "stream" was only the 2 KB of
                // stale FIFO content, which read as nusb failing.
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request,
                    value: 0,
                    index: 0,
                    data: &[value],
                },
                CONTROL_TIMEOUT,
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::debug!(
                    request = format_args!("0x{request:02x}"),
                    value,
                    attempt,
                    error = %e,
                    "fx2scope register write failed, retrying"
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("WRITE_REG_RETRIES >= 1").into())
}

/// Finds the bulk IN endpoint on `interface`'s current (default) alt
/// setting — see [`protocol`]'s doc comment: this firmware also exposes 7
/// isochronous alt settings on a different endpoint, but the device already
/// enumerates with alt 0 (bulk) active, so no `SET_INTERFACE` is needed
/// (see [`Fx2ScopeInstrument::prepare`]'s doc comment).
fn discover_bulk_in(interface: &nusb::Interface) -> Result<(u8, usize)> {
    let desc = interface
        .descriptor()
        .ok_or(NativeError::NoBulkInEndpoint {
            device: "fx2scope".to_string(),
            interface: protocol::INTERFACE_NUMBER,
        })?;
    desc.endpoints()
        .find(|ep| {
            ep.transfer_type() == nusb::descriptors::TransferType::Bulk
                && ep.direction() == nusb::transfer::Direction::In
        })
        .map(|ep| (ep.address(), ep.max_packet_size()))
        .ok_or(NativeError::NoBulkInEndpoint {
            device: "fx2scope".to_string(),
            interface: protocol::INTERFACE_NUMBER,
        })
}

#[async_trait]
impl Instrument for Fx2ScopeInstrument {
    fn label(&self) -> Option<String> {
        Some("FX2 2-channel oscilloscope".into())
    }

    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    async fn prepare(&mut self) -> CoreResult<()> {
        let bus = self.id.bus;
        let port_path = self.id.port_path.clone();

        let resolved = self.firmware_resolver.resolve(FIRMWARE_FILE)?;
        tracing::info!(
            device = %self.id,
            source = %resolved.source,
            bytes = resolved.firmware.len(),
            "uploading fx2scope firmware"
        );

        let info = fx2::find_at_path(bus, &port_path).await?.ok_or_else(|| {
            NativeError::DeviceNotFound {
                bus,
                port_path: port_path.clone(),
            }
        })?;
        let before_vid_pid = (info.vendor_id(), info.product_id());
        let device = fx2::open_device(&info).await.map_err(NativeError::Open)?;

        fx2::upload(&device, &resolved.firmware).await?;
        // The device disconnects on its own as the 8051 comes out of reset;
        // drop our handle rather than race that (fx2lafw::driver::prepare).
        drop(device);

        let reenumerated =
            fx2::wait_for_reenumeration(bus, &port_path, fx2::DEFAULT_REENUMERATION_TIMEOUT)
                .await?;
        let after_vid_pid = (reenumerated.vendor_id(), reenumerated.product_id());
        tracing::info!(
            device = %self.id,
            before = ?before_vid_pid,
            after = ?after_vid_pid,
            "re-enumerated after firmware upload"
        );
        self.id = self
            .id
            .clone()
            .with_vid_pid(after_vid_pid.0, after_vid_pid.1);

        let device = fx2::open_device(&reenumerated)
            .await
            .map_err(NativeError::Open)?;
        let interface = device
            .claim_interface(protocol::INTERFACE_NUMBER)
            .await
            .map_err(NativeError::Usb)?;
        // No explicit `SET_INTERFACE` here: the device enumerates with alt
        // setting 0 (bulk) already active, matching the reference
        // `libsigrok` driver (which never calls it either). An explicit
        // `set_alt_setting(0)` was tried during bring-up and made no
        // reliable difference to the streaming issue below — omitted to
        // match the one implementation known to work.
        let (bulk_in_addr, max_packet_size) = discover_bulk_in(&interface)?;
        tracing::info!(
            device = %self.id,
            endpoint = format_args!("0x{bulk_in_addr:02x}"),
            max_packet_size,
            "discovered bulk IN endpoint"
        );

        self.state = State::Ready(ReadyState {
            device,
            interface,
            bulk_in_addr,
            max_packet_size,
        });
        self.applied = None;
        Ok(())
    }

    async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<EffectiveAcq> {
        self.ready().map_err(CoreError::from)?;

        protocol::samplerate_reg(cfg.rate_hz).map_err(CoreError::from)?;
        if cfg.rate_hz > protocol::MAX_SUSTAINED_RATE_HZ {
            return Err(CoreError::Config(format!(
                "{} Hz exceeds the {} Hz this device sustains: this device always \
                 streams both channels interleaved (2 bytes/sample regardless of \
                 channel selection), so beyond this rate USB 2.0 bulk bandwidth is \
                 exceeded and the device sends no data",
                cfg.rate_hz,
                protocol::MAX_SUSTAINED_RATE_HZ
            )));
        }
        protocol::validate_channels(&cfg.channels).map_err(CoreError::from)?;
        // `samples` is ignored in continuous mode (SPEC §2.3.2): 0 is valid
        // there, since the caller has nothing meaningful to put in it.
        if !cfg.continuous && cfg.samples == 0 {
            return Err(CoreError::Config("samples must be > 0".to_string()));
        }
        if !cfg.ranges_v.is_empty() && cfg.ranges_v.len() != cfg.channels.len() {
            return Err(CoreError::Config(format!(
                "ranges_v has {} entries but {} channels are enabled",
                cfg.ranges_v.len(),
                cfg.channels.len()
            )));
        }

        // SPECIFICATION.md §2.3.1: "empty means a sensible default" — the
        // device's own power-on default (500 mV/div), applied per enabled
        // channel; otherwise snap each requested value to what the hardware
        // offers (§2.3.1: ranges are requestable, and the applied range is
        // returned, never assumed to be exactly the request).
        let ranges: Vec<Range> = if cfg.ranges_v.is_empty() {
            cfg.channels
                .iter()
                .map(|_| protocol::default_range())
                .collect()
        } else {
            cfg.ranges_v
                .iter()
                .map(|&v| protocol::nearest_range(v))
                .collect()
        };

        let mut effective_cfg = cfg.clone();
        effective_cfg.ranges_v = ranges.iter().map(|r| r.range_v).collect();

        let calibration = cfg
            .channels
            .iter()
            .zip(&ranges)
            .map(|(&ch, &range)| {
                let (scale, offset) = protocol::calibration_for(range);
                ChannelCalibration {
                    channel: ch,
                    scale,
                    offset,
                    unit: "V".to_string(),
                    range_v: Some(range.range_v),
                }
            })
            .collect();

        self.applied = Some(Applied {
            cfg: effective_cfg.clone(),
            ranges,
        });

        Ok(EffectiveAcq {
            cfg: effective_cfg,
            calibration,
        })
    }

    async fn start(&mut self, sink: SampleSink) -> CoreResult<CaptureId> {
        self.ready().map_err(CoreError::from)?;
        let applied = self.applied.clone().ok_or_else(|| {
            CoreError::Config("configure() must complete before start()".to_string())
        })?;

        if self.task.is_some() {
            self.stop().await?;
        }

        let rate_reg = protocol::samplerate_reg(applied.cfg.rate_hz).map_err(CoreError::from)?;
        // Both CH1 and CH2 range registers are written on every capture,
        // matching the reference driver's own behaviour (traced on the
        // bench: setting CH1's vdiv also rewrites CH2's, and vice versa —
        // see `protocol` module doc comment). A disabled channel gets the
        // device's default range: its bytes are still on the wire (§0xE4
        // doc comment) and discarded host-side, but the register must hold
        // some valid code regardless.
        let ch1_range = applied
            .cfg
            .channels
            .iter()
            .position(|&c| c == 0)
            .map(|i| applied.ranges[i])
            .unwrap_or_else(protocol::default_range);
        let ch2_range = applied
            .cfg
            .channels
            .iter()
            .position(|&c| c == 1)
            .map(|i| applied.ranges[i])
            .unwrap_or_else(protocol::default_range);

        {
            let ready = self.ready().map_err(CoreError::from)?;
            tracing::debug!(
                device = %self.id,
                rate_reg = format_args!("0x{rate_reg:02x}"),
                ch1 = ch1_range.code,
                ch2 = ch2_range.code,
                "writing fx2scope registers"
            );
            write_reg(&ready.device, protocol::CMD_SAMPLERATE, rate_reg)
                .await
                .map_err(CoreError::from)?;
            write_reg(&ready.device, protocol::CMD_VDIV_CH1, ch1_range.code)
                .await
                .map_err(CoreError::from)?;
            write_reg(&ready.device, protocol::CMD_VDIV_CH2, ch2_range.code)
                .await
                .map_err(CoreError::from)?;
        }

        let ready = self.ready().map_err(CoreError::from)?;
        let mut endpoint = ready
            .interface
            .endpoint::<Bulk, In>(ready.bulk_in_addr)
            .map_err(NativeError::Usb)
            .map_err(CoreError::from)?;

        let channels = applied.cfg.channels.clone();
        let channel_count = channels.len() as u8;
        let rate_hz = applied.cfg.rate_hz;
        let requested = applied.cfg.samples; // per-channel timepoints.
        let continuous = applied.cfg.continuous;
        let buf_len = buffer_len(rate_hz, ready.max_packet_size);
        let num_transfers = transfer_count(rate_hz, buf_len);
        let stall = stall_timeout(rate_hz, buf_len);
        let device_id = self.id.clone();

        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let stats = self.stats.clone();
        *stats.lock().expect("stats mutex poisoned") = AcquisitionStats {
            requested_samples: requested,
            continuous,
            ..Default::default()
        };

        // Reads go out BEFORE the start command, so one is pending from the
        // first byte. The reverse order left a window with nothing to read
        // into; at 16 MHz (32 MB/s) the 2 KB FIFO overruns in ~64 us, and a
        // capture lost ~170 samples near its start — measured on the bench
        // against the CAL square wave.
        for _ in 0..num_transfers {
            let buffer = endpoint.allocate(buf_len);
            endpoint.submit(buffer);
        }
        {
            let ready = self.ready().map_err(CoreError::from)?;
            write_reg(&ready.device, protocol::CMD_TRIGGER, 1)
                .await
                .map_err(CoreError::from)?;
        }

        let handle = tokio::spawn(async move {
            tracing::debug!(
                buf_len,
                num_transfers,
                rate_hz,
                ?stall,
                "starting fx2scope acquisition"
            );

            let mut received: u64 = 0; // per-channel timepoints.
            let mut dropped_transfers = 0usize;
            let mut max_pending = 0usize;
            let mut min_pending = usize::MAX;
            let mut stalled = false;
            let mut first_data: Option<Instant> = None;
            let mut last_data = Instant::now();
            let mut bytes_after_first: u64 = 0;
            let mut stale_to_skip = STALE_FIFO_BYTES;

            'acquire: while continuous || received < requested {
                if endpoint.pending() == 0 {
                    break;
                }
                // Cancellation and the stall timeout race every completion —
                // never checked only between completions — for exactly the
                // reason `fx2lafw::driver` documents: a silent device must
                // not park this loop inside `next_complete()` beyond `stop()`'s
                // reach. `next_complete` is cancel-safe (nusb 0.2 docs).
                let completion = tokio::select! {
                    biased;
                    _ = &mut cancel_rx => break 'acquire,
                    c = endpoint.next_complete() => c,
                    () = tokio::time::sleep(stall) => {
                        stalled = true;
                        tracing::error!(
                            device = %device_id, rate_hz, received, requested, ?stall,
                            "fx2scope stopped streaming; ending the acquisition"
                        );
                        break 'acquire;
                    }
                };
                let pending_after = endpoint.pending();
                max_pending = max_pending.max(pending_after);
                min_pending = min_pending.min(pending_after);

                tracing::trace!(
                    status = ?completion.status,
                    actual_len = completion.actual_len,
                    pending_after,
                    "fx2scope bulk completion"
                );
                match completion.status {
                    Ok(()) => {
                        let now = Instant::now();
                        if first_data.is_none() {
                            first_data = Some(now);
                        } else {
                            bytes_after_first += completion.actual_len as u64;
                        }
                        last_data = now;

                        // Drop the stale FIFO prefix before anything is kept.
                        let skip = stale_to_skip.min(completion.actual_len);
                        stale_to_skip -= skip;
                        let fresh = &completion.buffer[skip..completion.actual_len];

                        // Every timepoint is 2 raw bytes on the wire
                        // regardless of channel selection (protocol doc
                        // comment); trim to whole timepoints before
                        // decoding so `decode_interleaved` never sees a
                        // dangling half-pair from a mid-capture cutoff.
                        // Continuous: keep everything fresh — there is no
                        // `requested` ceiling to clamp to.
                        let want_bytes = if continuous {
                            fresh.len()
                        } else {
                            let remaining_timepoints = requested - received;
                            (remaining_timepoints * WIRE_BYTES_PER_SAMPLE).min(fresh.len() as u64)
                                as usize
                        };
                        let take = want_bytes - (want_bytes % 2);
                        if take > 0 {
                            let decoded = protocol::decode_interleaved(&fresh[..take], &channels);
                            let produced = decoded.len() as u64 / channel_count.max(1) as u64;
                            let t0_ns = (u128::from(received) * 1_000_000_000u128
                                / u128::from(rate_hz.max(1)))
                                as u64;
                            let chunk = SampleChunk {
                                encoding: Encoding::U8Interleaved,
                                channel_count,
                                sample_count: produced as u32,
                                t0_ns,
                                data: Bytes::from(decoded),
                            };
                            if sink.send(chunk).await.is_err() {
                                break 'acquire;
                            }
                            received += produced;
                        }
                        if continuous || received < requested {
                            endpoint.submit(completion.buffer);
                        }
                    }
                    Err(status) => {
                        dropped_transfers += 1;
                        tracing::error!(error = %status, "fx2scope bulk transfer failed");
                        if status == nusb::transfer::TransferError::Disconnected {
                            break 'acquire;
                        }
                        if status != nusb::transfer::TransferError::Cancelled
                            && (continuous || received < requested)
                        {
                            endpoint.submit(completion.buffer);
                        }
                    }
                }
            }

            endpoint.cancel_all();
            drain(&mut endpoint).await;

            let measured =
                first_data.and_then(|first| measured_rate(first, last_data, bytes_after_first));
            let rate_mismatch = measured.is_some_and(|m| !rate_within_tolerance(m, rate_hz));
            if let (true, Some(m)) = (rate_mismatch, measured) {
                tracing::error!(
                    device = %device_id, configured_hz = rate_hz, measured_hz = m,
                    "fx2scope sampled at the wrong rate; this capture's timebase is unreliable"
                );
            }

            let mut stats = stats.lock().expect("stats mutex poisoned");
            stats.received_samples = received;
            stats.dropped_transfers = dropped_transfers;
            stats.max_transfers_in_flight = max_pending;
            stats.min_transfers_in_flight = if min_pending == usize::MAX {
                0
            } else {
                min_pending
            };
            stats.stalled = stalled;
            stats.measured_rate_hz = measured;
            stats.rate_mismatch = rate_mismatch;
        });

        self.cancel = Some(cancel_tx);
        self.task = Some(handle);
        Ok(CaptureId::new())
    }

    async fn stop(&mut self) -> CoreResult<()> {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
                tracing::error!(device = %self.id, "acquisition task ignored cancellation; aborting it");
                task.abort();
            }
        }
        // SPECIFICATION.md's traced sequence stops the device itself too
        // (0xE3 <- 0), matching the reference driver, rather than merely
        // abandoning the host-side transfer loop.
        if let State::Ready(ready) = &self.state {
            if let Err(err) = write_reg(&ready.device, protocol::CMD_TRIGGER, 0).await {
                tracing::warn!(device = %self.id, %err, "failed to write stop (0xE3 <- 0)");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_timeout_has_a_floor_and_scales_with_slow_buffers() {
        assert_eq!(stall_timeout(16_000_000, 32_000), MIN_STALL_TIMEOUT);
        // 512 B at 2 bytes/sample and 20 Hz: 512/(2*20) = 12.8 s to fill;
        // 10x that is the bound.
        assert_eq!(stall_timeout(20, 512), Duration::from_secs_f64(128.0));
    }

    #[test]
    fn rate_measurement_ignores_short_captures() {
        let t = Instant::now();
        assert_eq!(measured_rate(t, t + Duration::from_millis(50), 2_000), None);
        // 2 MHz for 2 s -> 4,000,000 timepoints -> 8,000,000 wire bytes.
        let m = measured_rate(t, t + Duration::from_secs(2), 8_000_000).unwrap();
        assert!((m - 2_000_000.0).abs() < 1.0);
    }

    #[test]
    fn rate_tolerance_matches_fx2lafws_bounds() {
        assert!(!rate_within_tolerance(20_009.0, 25_000));
        assert!(rate_within_tolerance(972_534.0, 1_000_000));
    }

    #[test]
    fn buffer_len_is_a_packet_multiple_of_about_eight_ms() {
        // 16 MHz, both channels always streamed: 32 000 B/ms -> ~256 KB.
        let len = buffer_len(16_000_000, 512);
        assert_eq!(len % 512, 0);
        assert!((250_000..=260_000).contains(&len), "{len}");
        assert_eq!(buffer_len(100_000, 512) % 512, 0);
    }

    #[test]
    fn several_transfers_stay_in_flight_within_the_usbfs_budget() {
        for rate in [100_000u64, 1_000_000, 16_000_000] {
            let len = buffer_len(rate, 512);
            let n = transfer_count(rate, len);
            assert!(
                n >= MIN_TRANSFERS,
                "a read must always be pending at {rate} Hz"
            );
            assert!(
                n * len <= MAX_INFLIGHT_BYTES.max(MIN_TRANSFERS * len),
                "{rate} Hz"
            );
        }
    }
}

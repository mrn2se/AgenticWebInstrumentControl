//! The `fx2lafw` [`Instrument`] — an 8-channel digital logic analyzer on a
//! Cypress FX2 (SPECIFICATION.md §2.5), reached at `0925:3881`.
//!
//! SPECIFICATION.md §5.1: `0925:3881` is also how an FX2 oscilloscope enumerates
//! when its physical mode switch is set to "logic", and neither device
//! reports a serial number. This driver claims whatever answers at the bus
//! path it was given — that is correct per §5.1 ("the UI must label by
//! location, not by model") — it never asserts the attached hardware really
//! is a standalone fx2lafw logic analyzer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awic_core::{
    AcqConfig, Capability, CaptureId, CoreError, DeviceId, EffectiveAcq, Instrument,
    Result as CoreResult, SampleChunk, SampleSink,
};
use awic_proto::frame::Encoding;
use bytes::Bytes;
use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::error::{NativeError, Result};
use crate::firmware::FirmwareResolver;
use crate::fx2;

use super::protocol::{self, FirmwareVersion, StartCommand, CHANNEL_COUNT};

/// The Debian `sigrok-firmware-fx2lafw` package's filename for this exact
/// device (SPECIFICATION.md §2.5, confirmed against `libsigrok`'s own
/// device profile table, which pairs vid:pid `0925:3881` with this name).
pub const FIRMWARE_FILE: &str = "fx2lafw-saleae-logic.fw";

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Target duration of samples per USB buffer. Small enough to keep latency
/// reasonable, large enough to stay well clear of per-transfer overhead.
const TARGET_CHUNK_MS: u64 = 8;
/// Target total data buffered in flight with the kernel/host controller
/// before our consumer loop must have serviced a completion — this is the
/// slack SPECIFICATION.md §2.6 calls for ("several bulk transfers in
/// flight"): as long as this many milliseconds of already-submitted buffers
/// exist, a momentary stall in our own task (e.g. the `SampleSink` applying
/// backpressure) does not lose samples at the device's onboard FIFO.
const TARGET_INFLIGHT_MS: u64 = 250;
const MIN_TRANSFERS: usize = 4;
const MAX_TRANSFERS: usize = 24;

/// Floor for the streaming stall timeout: how long the acquisition loop waits
/// for *any* transfer to complete before declaring the device stalled.
///
/// Without a bound, a device that stops streaming — fx2lafw does exactly
/// that when its FIFO overruns, e.g. 8 channels at 48 MHz is 48 MB/s, beyond
/// what USB 2.0 bulk sustains — leaves the loop awaiting forever, and with it
/// `stop()` and every caller. That was observed on the bench: a hardware test
/// hung for over four hours holding the device.
const MIN_STALL_TIMEOUT: Duration = Duration::from_secs(1);
/// Stall = no completion for this many buffer-fill times (floored above).
const STALL_BUFFERS: u32 = 10;
/// Bound on draining cancelled transfers; they normally complete at once.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// `stop()` waits this long for the acquisition task, then aborts it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Captures shorter than this are not rate-checked: over a few ms, USB
/// completion jitter dominates and the measurement means nothing.
const RATE_CHECK_MIN_SPAN: Duration = Duration::from_millis(300);
/// Allowed deviation of the measured sample rate from the configured one.
/// Every ladder rate is an exact divisor of the 30/48 MHz clock, and bench
/// measurements land within ~3%, so 10% is generous — yet still catches the
/// firmware's rate-change lag even between neighbouring rates (20 vs 25 kHz
/// is 20% off).
const RATE_TOLERANCE: f64 = 0.10;

/// How many times an acquisition that has delivered *nothing* may be started
/// again before the stall is reported to the caller (B-26). One is what the
/// bench needs — the retry has never failed there — and a second is kept for
/// the case where the device is briefly wedged rather than merely silent.
/// Each attempt costs one stall timeout, which stays inside `capture`'s
/// 5-second minimum default.
const MAX_SILENT_RESTARTS: u8 = 2;

/// How long to wait for any completion before calling the stream stalled.
fn stall_timeout(rate_hz: u64, buf_len: usize) -> Duration {
    let fill = Duration::from_secs_f64(buf_len as f64 / rate_hz.max(1) as f64);
    (fill * STALL_BUFFERS).max(MIN_STALL_TIMEOUT)
}

/// What [`Fx2lafwInstrument::start`] must do before committing an
/// acquisition to the device, given how the previous acquisition ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preflight {
    /// Same rate as last time, and the last acquisition streamed cleanly (or
    /// was stopped on purpose).
    Nothing,
    /// A priming acquisition first: the rate changed (the firmware's
    /// rate-change lag), or the last start received nothing at all (B-20).
    Prime,
    /// A firmware reset first, then a priming acquisition: the last
    /// acquisition streamed and then stalled (B-25).
    Reset,
}

/// A device that streamed and then went silent has, on this bench, always
/// been a device in the degraded state only an 8051 reset clears (live test
/// B-25): after one mid-stream stall at the top rate, *every* later
/// acquisition stalled too — some twenty in a row, 8-450 ms in — until a
/// restart re-uploaded the firmware, after which 24 MHz ran clean again. A
/// silent start (B-20) is the other face of the same state; the priming
/// acquisition catches that one, but not this one, because here priming
/// succeeds. So a stall with data is answered with the reset up front.
fn preflight(previous: &AcquisitionStats, rate_changed: bool) -> Preflight {
    if previous.stalled && previous.received_samples > 0 {
        Preflight::Reset
    } else if rate_changed || (previous.stalled && previous.received_samples == 0) {
        Preflight::Prime
    } else {
        Preflight::Nothing
    }
}

/// Samples per second actually delivered between the first and the last
/// completion. The first completion's own samples are excluded: its arrival
/// marks the *end* of its fill, so only what arrived after it spans
/// `last - first`. `None` when the span is too short to mean anything.
fn measured_rate(first: Instant, last: Instant, produced_after_first: u64) -> Option<f64> {
    let span = last.saturating_duration_since(first);
    (span >= RATE_CHECK_MIN_SPAN && produced_after_first > 0)
        .then(|| produced_after_first as f64 / span.as_secs_f64())
}

fn rate_within_tolerance(measured: f64, configured: u64) -> bool {
    (measured / configured.max(1) as f64 - 1.0).abs() <= RATE_TOLERANCE
}

fn buffer_len(rate_hz: u64, max_packet_size: usize) -> usize {
    let bytes_per_ms = (rate_hz / 1000).max(1) as usize; // 1 byte per sample.
    let wanted = TARGET_CHUNK_MS as usize * bytes_per_ms;
    let mp = max_packet_size.max(1);
    // IN transfers must request a nonzero multiple of max_packet_size.
    let rounded = wanted.div_ceil(mp) * mp;
    rounded.max(mp)
}

fn transfer_count(rate_hz: u64, buf_len: usize) -> usize {
    let bytes_per_ms = (rate_hz / 1000).max(1) as usize;
    let target_bytes = TARGET_INFLIGHT_MS as usize * bytes_per_ms;
    (target_bytes / buf_len.max(1)).clamp(MIN_TRANSFERS, MAX_TRANSFERS)
}

/// Diagnostics from the most recently completed (or in-progress)
/// acquisition. Not part of the `Instrument` trait — core names no
/// driver-specific type — but exposed on the concrete driver so hardware
/// tests can assert "zero drops" directly instead of inferring it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcquisitionStats {
    pub requested_samples: u64,
    pub received_samples: u64,
    /// Transfers that completed with a non-`Ok` status. SPECIFICATION.md
    /// §2.6/R3: this must be `0` for a clean high-rate capture.
    pub dropped_transfers: usize,
    pub max_transfers_in_flight: usize,
    pub min_transfers_in_flight: usize,
    /// The stream went silent for longer than the stall timeout and the
    /// acquisition was ended rather than awaited forever.
    pub stalled: bool,
    /// A throwaway acquisition ran first to absorb the firmware's
    /// rate-change lag (see `Fx2lafwInstrument::last_rate_hz`).
    pub primed: bool,
    /// The device was reset (firmware re-upload) before this acquisition:
    /// the previous one stalled mid-stream (B-25), or two starts in a row
    /// received nothing (B-20).
    pub reset_before: bool,
    /// How many times the acquisition was silently restarted because the
    /// device sent nothing at all (B-26). Zero for a normal capture; the
    /// samples that did arrive are unaffected, since a restart only happens
    /// while nothing has been delivered yet.
    pub silent_restarts: u8,
    /// This acquisition ran until `stop()` rather than for a fixed sample
    /// count (SPECIFICATION.md §2.3.2) — `requested_samples` is then
    /// meaningless (`samples` is ignored) and callers must not compare
    /// `received_samples` against it.
    pub continuous: bool,
    /// Sample rate measured from completion timing, when the capture was
    /// long enough to measure (see [`RATE_CHECK_MIN_SPAN`]).
    pub measured_rate_hz: Option<f64>,
    /// The measured rate disagreed with the configured one by more than
    /// [`RATE_TOLERANCE`]: the capture's timebase cannot be trusted.
    pub rate_mismatch: bool,
}

struct ReadyState {
    device: nusb::Device,
    interface: nusb::Interface,
    bulk_in_addr: u8,
    max_packet_size: usize,
    fw_version: FirmwareVersion,
    revid: u8,
}

enum State {
    Unprepared,
    Ready(ReadyState),
}

pub struct Fx2lafwInstrument {
    id: DeviceId,
    capabilities: Vec<Capability>,
    firmware_resolver: Arc<FirmwareResolver>,
    state: State,
    acq: Option<AcqConfig>,
    channel_mask: u8,
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    stats: Arc<Mutex<AcquisitionStats>>,
    /// Sample rate of the last acquisition run on the currently loaded
    /// firmware instance, if any.
    ///
    /// **Firmware behaviour, measured on the bench and reproduced with the
    /// reference implementation (`sigrok-cli`), not documented anywhere this
    /// driver consulted:** after an acquisition on fx2lafw's 30 MHz clock
    /// (the slow rates, e.g. 20 kHz), the *first* acquisition with a changed
    /// rate still runs at the previous rate. The one after it is correct.
    ///
    /// ```text
    /// sigrok-cli, firmware already loaded:
    ///   20k -> 1M : first capture ran at ~20 kHz, second at 1 MHz
    ///   20k -> 25k: first capture ran at ~20 kHz, second at 25 kHz
    ///   1M  -> 2M : first capture already correct
    ///   2M  -> 20k: first capture already correct
    /// ```
    ///
    /// Re-uploading the firmware does not cure it — that was tried, and gave
    /// 459 kHz for a requested 1 MHz: a CPUCS soft reset restarts the 8051
    /// but does not clear the FX2's clock/GPIF configuration. So `start()`
    /// instead runs a short throwaway acquisition whenever the rate differs
    /// from the last one *known* to have run — including when none is known
    /// (`None`, after `prepare()`). That absorbs the lag regardless of which
    /// clock the rates use or what ran before this process, at the cost of
    /// one USB packet (~26 ms at worst, 512 B at 20 kHz).
    last_rate_hz: Option<u64>,
}

impl Fx2lafwInstrument {
    pub fn new(id: DeviceId, firmware_resolver: Arc<FirmwareResolver>) -> Self {
        Self {
            id,
            capabilities: vec![Capability::Digital {
                channels: CHANNEL_COUNT,
                max_rate_hz: protocol::MAX_SUSTAINED_RATE_HZ,
                rates_hz: protocol::sustained_rates_hz(),
            }],
            firmware_resolver,
            state: State::Unprepared,
            acq: None,
            channel_mask: 0,
            cancel: None,
            task: None,
            stats: Arc::new(Mutex::new(AcquisitionStats::default())),
            last_rate_hz: None,
        }
    }

    /// Diagnostics for the last `start()`/`stop()` cycle. See
    /// [`AcquisitionStats`].
    pub fn last_stats(&self) -> AcquisitionStats {
        self.stats.lock().expect("stats mutex poisoned").clone()
    }

    /// The firmware version/revision queried during `prepare()`, once ready.
    pub fn firmware_info(&self) -> Option<(FirmwareVersion, u8)> {
        match &self.state {
            State::Ready(r) => Some((r.fw_version, r.revid)),
            State::Unprepared => None,
        }
    }

    /// A short throwaway acquisition at `rate_hz`, discarded. Absorbs the
    /// firmware's rate-change lag — see [`Self::last_rate_hz`] — and doubles
    /// as the check that the device starts at all at this rate: `Ok(false)`
    /// means it sent nothing within the stall floor. Bounded either way.
    async fn prime(&self, rate_hz: u64) -> Result<bool> {
        let ready = self.ready()?;
        send_start(&ready.device, protocol::start_command(rate_hz)?).await?;
        let mut endpoint = ready
            .interface
            .endpoint::<Bulk, In>(ready.bulk_in_addr)
            .map_err(NativeError::Usb)?;
        let buffer = endpoint.allocate(ready.max_packet_size.max(1));
        endpoint.submit(buffer);
        let got = tokio::time::timeout(MIN_STALL_TIMEOUT, endpoint.next_complete()).await;
        let started = matches!(&got, Ok(completion) if completion.status.is_ok());
        if !started {
            tracing::warn!(device = %self.id, rate_hz, "priming acquisition received no data");
        }
        endpoint.cancel_all();
        drain(&mut endpoint).await;
        Ok(started)
    }

    fn ready(&self) -> Result<&ReadyState> {
        match &self.state {
            State::Ready(r) => Ok(r),
            State::Unprepared => Err(NativeError::NotPrepared("configure")),
        }
    }
}

/// Waits for cancelled transfers to come back, bounded by [`DRAIN_TIMEOUT`]:
/// an endpoint that never returns them must not hang the caller.
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
            "fx2lafw transfers did not drain"
        );
    }
}

async fn get_fw_version(device: &nusb::Device) -> Result<FirmwareVersion> {
    let data = device
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: protocol::CMD_GET_FW_VERSION,
                value: 0,
                index: 0,
                length: 2,
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    FirmwareVersion::parse(&data)
}

async fn get_revid(device: &nusb::Device) -> Result<u8> {
    let data = device
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: protocol::CMD_GET_REVID_VERSION,
                value: 0,
                index: 0,
                length: 1,
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    data.first()
        .copied()
        .ok_or(NativeError::Transfer(nusb::transfer::TransferError::Fault))
}

async fn send_start(device: &nusb::Device, cmd: StartCommand) -> Result<()> {
    let payload = cmd.to_bytes();
    device
        .control_out(
            ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: protocol::CMD_START,
                value: 0,
                index: 0,
                data: &payload,
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    Ok(())
}

/// Finds the (sole) bulk IN endpoint on `interface`'s current alt setting.
/// Discovered dynamically rather than hardcoded: SPECIFICATION.md §2.5 says
/// `0x86`, but the endpoint a given `.fw` image actually uses is a property
/// of that firmware image, not something host software should assume —
/// reading it from the live device's own descriptor is the only way to be
/// sure, and this driver logs what it finds either way.
fn discover_bulk_in(interface: &nusb::Interface) -> Result<(u8, usize)> {
    let desc = interface
        .descriptor()
        .ok_or(NativeError::NoBulkInEndpoint {
            device: "fx2lafw".to_string(),
            interface: protocol::INTERFACE_NUMBER,
        })?;
    desc.endpoints()
        .find(|ep| {
            ep.transfer_type() == nusb::descriptors::TransferType::Bulk
                && ep.direction() == nusb::transfer::Direction::In
        })
        .map(|ep| (ep.address(), ep.max_packet_size()))
        .ok_or(NativeError::NoBulkInEndpoint {
            device: "fx2lafw".to_string(),
            interface: protocol::INTERFACE_NUMBER,
        })
}

#[async_trait]
impl Instrument for Fx2lafwInstrument {
    fn label(&self) -> Option<String> {
        Some("fx2lafw 8-channel logic analyzer".into())
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
            "uploading fx2lafw firmware"
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
        // The device is about to disconnect on its own as the 8051 comes
        // out of reset; drop our handle to it rather than race that.
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
        let (bulk_in_addr, max_packet_size) = discover_bulk_in(&interface)?;
        tracing::info!(
            device = %self.id,
            endpoint = format_args!("0x{bulk_in_addr:02x}"),
            max_packet_size,
            "discovered bulk IN endpoint"
        );

        let fw_version = get_fw_version(&device).await?;
        if fw_version.major != protocol::EXPECTED_FW_VERSION_MAJOR {
            tracing::warn!(
                device = %self.id,
                major = fw_version.major,
                minor = fw_version.minor,
                expected_major = protocol::EXPECTED_FW_VERSION_MAJOR,
                "fx2lafw firmware major version differs from what this driver was built against"
            );
        } else {
            tracing::info!(device = %self.id, major = fw_version.major, minor = fw_version.minor, "fx2lafw firmware version");
        }
        let revid = get_revid(&device).await?;
        tracing::info!(device = %self.id, revid, "fx2lafw REVID");

        self.state = State::Ready(ReadyState {
            device,
            interface,
            bulk_in_addr,
            max_packet_size,
            fw_version,
            revid,
        });
        // A freshly (re)loaded firmware instance has never been started —
        // see the `armed` field's doc comment for why this matters.
        // Unknown, not "clean": a re-upload does not reset the FX2's clock/GPIF
        // state, so the next acquisition primes (see `last_rate_hz`).
        self.last_rate_hz = None;
        Ok(())
    }

    /// True when the previous acquisition streamed and then stalled, which is
    /// the case `configure` answers with a firmware re-upload and a
    /// re-enumeration (~2-4 s). The server marks the device `flashing` for
    /// that, rather than leaving it `ready` while it is being re-flashed
    /// (B-63).
    fn pending_reset(&self) -> bool {
        let previous = self.stats.lock().expect("stats mutex poisoned").clone();
        preflight(&previous, false) == Preflight::Reset
    }

    async fn configure(&mut self, cfg: AcqConfig) -> CoreResult<EffectiveAcq> {
        self.ready().map_err(CoreError::from)?;

        protocol::start_command(cfg.rate_hz).map_err(CoreError::from)?;
        if cfg.rate_hz > protocol::MAX_SUSTAINED_RATE_HZ {
            return Err(CoreError::Config(format!(
                "{} Hz is above the {} Hz this device sustains on a shared USB path: 8 channels \
                 at a higher rate overrun the bulk bandwidth and the device either sends nothing \
                 or stops part-way, costing a firmware re-upload before the next acquisition \
                 (B-63). Capture at {} Hz or below; the rates this device offers are in its \
                 capability's `rates_hz`",
                cfg.rate_hz,
                protocol::MAX_SUSTAINED_RATE_HZ,
                protocol::MAX_SUSTAINED_RATE_HZ,
            )));
        }
        let mask = protocol::channel_mask(&cfg.channels).map_err(CoreError::from)?;
        // `samples` is ignored in continuous mode (SPEC §2.3.2): 0 is valid
        // there, since the caller has nothing meaningful to put in it.
        if !cfg.continuous && cfg.samples == 0 {
            return Err(CoreError::Config("samples must be > 0".to_string()));
        }

        self.channel_mask = mask;
        self.acq = Some(cfg.clone());
        // Only exact ladder rates are accepted (`start_command` rejects the
        // rest), so the request is what runs: logic-level calibration.
        Ok(EffectiveAcq::digital(cfg))
    }

    async fn start(&mut self, sink: SampleSink) -> CoreResult<CaptureId> {
        self.ready().map_err(CoreError::from)?;
        let cfg = self.acq.clone().ok_or_else(|| {
            CoreError::Config("configure() must complete before start()".to_string())
        })?;

        // Two acquisitions must never share the endpoint.
        if self.task.is_some() {
            self.stop().await?;
        }

        // `last_rate_hz` includes `None` — the first acquisition after
        // `prepare()`. The lag state lives in the *device*, not in this
        // process: it survives a firmware re-upload and even a different
        // process (sigrok-cli showed it across invocations). A fresh
        // instance cannot know what rate the hardware last ran at, so it
        // primes too. See [`preflight`] for the stall cases.
        let previous = self.stats.lock().expect("stats mutex poisoned").clone();
        let plan = preflight(&previous, self.last_rate_hz != Some(cfg.rate_hz));
        let mut reset_before = plan == Preflight::Reset;
        if plan == Preflight::Reset {
            tracing::warn!(
                device = %self.id,
                rate_hz = cfg.rate_hz,
                stalled_after = previous.received_samples,
                "the previous acquisition streamed and then stalled; resetting the device \
                 (firmware re-upload) before the next one"
            );
            self.last_rate_hz = None;
            self.prepare().await?;
        }
        let primed = plan != Preflight::Nothing;
        if primed {
            tracing::debug!(device = %self.id, rate_hz = cfg.rate_hz, "priming after rate change");
            let mut started = self.prime(cfg.rate_hz).await.map_err(CoreError::from)?;
            if !started {
                // One retry first: the FX2 sometimes takes the second
                // 8-byte command after a FIFO overrun left by the previous
                // acquisition.
                started = self.prime(cfg.rate_hz).await.map_err(CoreError::from)?;
            }
            if !started {
                // Still nothing. The device is wedged, not merely busy: its
                // GPIF has stopped and no command restarts it, while lower
                // rates keep working — which is what made this look like "the
                // bus cannot carry 24 MHz" on the bench (live test B-20).
                // Only a reset of the 8051 clears it, which is exactly what
                // uploading the firmware does, so re-prepare and try once
                // more before giving up. Costs a re-enumeration (~2-4 s)
                // against a session that otherwise never streams again at
                // this rate until the server is restarted.
                tracing::warn!(
                    device = %self.id, rate_hz = cfg.rate_hz,
                    "no data at this rate after two starts; resetting the device \
                     (firmware re-upload) and retrying"
                );
                self.last_rate_hz = None;
                self.prepare().await?;
                reset_before = true;
                started = self.prime(cfg.rate_hz).await.map_err(CoreError::from)?;
                if started {
                    tracing::info!(
                        device = %self.id, rate_hz = cfg.rate_hz,
                        "the device streams again after the reset"
                    );
                }
            }
            if !started {
                self.last_rate_hz = None;
                let advice = protocol::next_lower_rate_hz(cfg.rate_hz).map_or_else(
                    || "no lower rate is available on this device".to_string(),
                    |lower| format!("try {} MHz", lower as f64 / 1e6),
                );
                return Err(CoreError::Device(format!(
                    "the logic analyzer sent nothing at {:.0} MHz within {:.0?}, twice, and again \
                     after a device reset: its 4 KB FIFO overruns before the first poll when the \
                     USB path cannot carry 8 bits x the sample rate. The rate is on the device's \
                     ladder, so this is about what this bus sustains right now — {advice}",
                    cfg.rate_hz as f64 / 1e6,
                    MIN_STALL_TIMEOUT,
                )));
            }
        }

        let start_cmd = protocol::start_command(cfg.rate_hz).map_err(CoreError::from)?;
        {
            let ready = self.ready().map_err(CoreError::from)?;
            send_start(&ready.device, start_cmd)
                .await
                .map_err(CoreError::from)?;
        }
        // A handle for the task's own use: a silent start is restarted from
        // inside the acquisition loop, which needs to re-send `start_cmd`.
        let restart_device = self.ready().map_err(CoreError::from)?.device.clone();
        self.last_rate_hz = Some(cfg.rate_hz);

        let ready = self.ready().map_err(CoreError::from)?;
        let mut endpoint = ready
            .interface
            .endpoint::<Bulk, In>(ready.bulk_in_addr)
            .map_err(NativeError::Usb)
            .map_err(CoreError::from)?;

        let mask = self.channel_mask;
        let channel_count = cfg.channels.len().max(1) as u8;
        let rate_hz = cfg.rate_hz;
        let requested = cfg.samples;
        let continuous = cfg.continuous;
        let buf_len = buffer_len(rate_hz, ready.max_packet_size);
        let num_transfers = transfer_count(rate_hz, buf_len);
        let stall = stall_timeout(rate_hz, buf_len);
        let device = self.id.clone();

        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let stats = self.stats.clone();
        *stats.lock().expect("stats mutex poisoned") = AcquisitionStats {
            requested_samples: requested,
            primed,
            reset_before,
            continuous,
            ..Default::default()
        };
        let restart_stats = stats.clone();

        let handle = tokio::spawn(async move {
            tracing::debug!(
                buf_len,
                num_transfers,
                rate_hz,
                ?stall,
                "starting fx2lafw acquisition"
            );

            for _ in 0..num_transfers {
                let buffer = endpoint.allocate(buf_len);
                endpoint.submit(buffer);
            }

            let mut received: u64 = 0;
            let mut dropped_transfers = 0usize;
            let mut max_pending = 0usize;
            let mut min_pending = usize::MAX;
            let mut stalled = false;
            let mut silent_restarts: u8 = 0;
            // Rate measurement: time of the first data, and every byte the
            // device produced after it (full buffers, not the trimmed `take`).
            let mut first_data: Option<Instant> = None;
            let mut last_data = Instant::now();
            let mut produced_after_first: u64 = 0;

            'acquire: while continuous || received < requested {
                if endpoint.pending() == 0 {
                    // Every submitted transfer already failed. Stop rather
                    // than spin.
                    break;
                }
                // Cancellation and the stall timeout are raced against the
                // completion, never checked only between completions: a
                // silent device would otherwise park this loop inside
                // `next_complete()` for good, out of reach of `stop()`.
                // `next_complete` is cancel-safe (nusb 0.2 docs).
                let completion = tokio::select! {
                    biased;
                    _ = &mut cancel_rx => break 'acquire,
                    c = endpoint.next_complete() => c,
                    () = tokio::time::sleep(stall) => {
                        // B-26: at the top of the rate ladder a start now and
                        // then produces nothing at all. The command is
                        // accepted, the device is not wedged — the very next
                        // start streams every sample — and priming first does
                        // not prevent it: measured on the bench, one capture
                        // in ten to twenty at 24 MHz, primed or not, between
                        // neighbours that delivered all 12 M samples with zero
                        // dropped transfers. Nothing has reached the sink yet,
                        // so there is no partial timeline and no data to lose:
                        // start it again rather than hand the caller an error
                        // it can only answer by retrying itself. A stall
                        // *after* data is the degraded state of B-25 and still
                        // ends the acquisition.
                        if received == 0 && silent_restarts < MAX_SILENT_RESTARTS {
                            silent_restarts += 1;
                            tracing::warn!(
                                device = %device, rate_hz, ?stall, silent_restarts,
                                "fx2lafw sent nothing at all; restarting the acquisition"
                            );
                            endpoint.cancel_all();
                            drain(&mut endpoint).await;
                            match send_start(&restart_device, start_cmd).await {
                                Ok(()) => {
                                    restart_stats
                                        .lock()
                                        .expect("stats mutex poisoned")
                                        .silent_restarts = silent_restarts;
                                    for _ in 0..num_transfers {
                                        let buffer = endpoint.allocate(buf_len);
                                        endpoint.submit(buffer);
                                    }
                                    last_data = Instant::now();
                                    continue 'acquire;
                                }
                                Err(err) => tracing::error!(
                                    device = %device, %err,
                                    "could not re-send the start command"
                                ),
                            }
                        }
                        stalled = true;
                        // Facts only (B-64). The old message asserted two
                        // things this code cannot see: that `configure`
                        // refuses every unsustainable rate - it did not, 24 MHz
                        // was offered until B-63 - and that another device was
                        // streaming behind the same hub, which on the bench had
                        // been idle for minutes in most of these. A driver
                        // knows its own numbers; what else is on the bus is the
                        // server's to report.
                        tracing::error!(
                            device = %device,
                            rate_hz,
                            received,
                            wanted = if continuous {
                                "continuous".to_string()
                            } else {
                                requested.to_string()
                            },
                            ?stall,
                            silent_restarts,
                            "fx2lafw stopped streaming after {received} of {} samples at {rate_hz} Hz; \
                             ending the acquisition. The device is not wedged: the next acquisition \
                             re-flashes it and usually streams. A rate near this device's ceiling \
                             on a USB path shared with anything else is the usual cause",
                            if continuous { "an unbounded".to_string() } else { requested.to_string() }
                        );
                        break 'acquire;
                    }
                };
                let pending_after = endpoint.pending();
                max_pending = max_pending.max(pending_after);
                min_pending = min_pending.min(pending_after);

                match completion.status {
                    Ok(()) => {
                        let now = Instant::now();
                        if first_data.is_none() {
                            first_data = Some(now);
                        } else {
                            produced_after_first += completion.actual_len as u64;
                        }
                        last_data = now;

                        // Continuous: keep everything the device sent —
                        // there is no `requested` ceiling to clamp to.
                        let take = if continuous {
                            completion.actual_len
                        } else {
                            let remaining = requested - received;
                            (completion.actual_len as u64).min(remaining) as usize
                        };
                        if take > 0 {
                            let masked = protocol::decode_samples(&completion.buffer[..take], mask);
                            let t0_ns = (u128::from(received) * 1_000_000_000u128
                                / u128::from(rate_hz.max(1)))
                                as u64;
                            let chunk = SampleChunk {
                                encoding: Encoding::PackedDigital,
                                channel_count,
                                sample_count: take as u32,
                                t0_ns,
                                data: Bytes::from(masked),
                            };
                            if sink.send(chunk).await.is_err() {
                                // Receiver gone: capture torn down downstream.
                                break 'acquire;
                            }
                            received += take as u64;
                        }
                        if continuous || received < requested {
                            endpoint.submit(completion.buffer);
                        }
                    }
                    Err(status) => {
                        dropped_transfers += 1;
                        tracing::error!(error = %status, "fx2lafw bulk transfer failed");
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
                first_data.and_then(|first| measured_rate(first, last_data, produced_after_first));
            let rate_mismatch = measured.is_some_and(|m| !rate_within_tolerance(m, rate_hz));
            if let (true, Some(m)) = (rate_mismatch, measured) {
                // The data is not merely noisy: its timebase is wrong, and
                // every time-derived measurement on it would be too.
                tracing::error!(
                    device = %device, configured_hz = rate_hz, measured_hz = m,
                    "fx2lafw sampled at the wrong rate; this capture's timebase is unreliable"
                );
            }

            let mut stats = stats.lock().expect("stats mutex poisoned");
            stats.silent_restarts = silent_restarts;
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
            // The loop races this against every completion, so it lands even
            // while the device is silent.
            let _ = cancel.send(());
        }
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
                // Should be unreachable given the raced cancellation, but a
                // stop() that can hang is the failure this code exists to
                // remove — so there is no path where it does.
                tracing::error!(device = %self.id, "acquisition task ignored cancellation; aborting it");
                task.abort();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B-63: the server marks a device `flashing` while the driver re-uploads
    /// its firmware, and `pending_reset` is how it knows that is about to
    /// happen — the same condition `configure` acts on, so the two cannot
    /// disagree.
    #[test]
    fn pending_reset_matches_the_preflight_decision() {
        // Streamed and then stalled: the next configure re-flashes.
        assert_eq!(preflight(&ended(true, 1_152_000), false), Preflight::Reset);
        // Nothing at all, or a clean run: no re-flash, so no `flashing`.
        assert_eq!(preflight(&ended(true, 0), false), Preflight::Prime);
        assert_eq!(preflight(&ended(false, 12_000), false), Preflight::Nothing);
        // A rate change primes but does not re-flash.
        assert_eq!(preflight(&ended(false, 12_000), true), Preflight::Prime);
    }

    fn ended(stalled: bool, received_samples: u64) -> AcquisitionStats {
        AcquisitionStats {
            stalled,
            received_samples,
            ..Default::default()
        }
    }

    #[test]
    fn a_clean_acquisition_at_the_same_rate_needs_nothing() {
        assert_eq!(
            preflight(&ended(false, 1_000_000), false),
            Preflight::Nothing
        );
        // Stopped on purpose, part-way: not a stall.
        assert_eq!(preflight(&ended(false, 5), false), Preflight::Nothing);
    }

    #[test]
    fn a_rate_change_or_a_silent_start_primes() {
        assert_eq!(preflight(&ended(false, 1_000_000), true), Preflight::Prime);
        assert_eq!(preflight(&ended(true, 0), false), Preflight::Prime);
        assert_eq!(
            preflight(&AcquisitionStats::default(), true),
            Preflight::Prime
        );
    }

    #[test]
    fn a_stall_after_data_resets_the_device_first() {
        // B-25: streamed 2.1 M samples, then went silent.
        assert_eq!(preflight(&ended(true, 2_112_000), false), Preflight::Reset);
        // Even when the rate changes too: the reset comes first.
        assert_eq!(preflight(&ended(true, 1), true), Preflight::Reset);
    }

    #[test]
    fn stall_timeout_has_a_floor_and_scales_with_slow_buffers() {
        assert_eq!(stall_timeout(24_000_000, 192_000), MIN_STALL_TIMEOUT);
        // 512 B at 20 Hz takes 25.6 s to fill; ten of those is the bound.
        assert_eq!(stall_timeout(20, 512), Duration::from_secs_f64(256.0));
    }

    #[test]
    fn rate_measurement_ignores_short_captures() {
        let t = Instant::now();
        assert_eq!(measured_rate(t, t + Duration::from_millis(50), 1_000), None);
        let m = measured_rate(t, t + Duration::from_secs(2), 2_000_000).unwrap();
        assert!((m - 1_000_000.0).abs() < 1.0);
    }

    #[test]
    fn rate_tolerance_catches_the_neighbouring_rate_lag() {
        // The firmware lag observed on the bench: asked for 25 kHz, got 20.
        assert!(!rate_within_tolerance(20_009.0, 25_000));
        // Bench-typical honest measurement noise passes.
        assert!(rate_within_tolerance(972_534.0, 1_000_000));
        assert!(rate_within_tolerance(19_720.0, 20_000));
    }

    #[test]
    fn buffer_len_is_a_multiple_of_max_packet_size() {
        for rate in [20_000u64, 1_000_000, 48_000_000] {
            for mp in [64usize, 512] {
                let len = buffer_len(rate, mp);
                assert_eq!(len % mp, 0, "rate={rate} mp={mp} len={len}");
                assert!(len > 0);
            }
        }
    }

    #[test]
    fn transfer_count_is_within_documented_bounds() {
        for rate in [20_000u64, 1_000_000, 48_000_000] {
            let len = buffer_len(rate, 512);
            let n = transfer_count(rate, len);
            assert!(
                (MIN_TRANSFERS..=MAX_TRANSFERS).contains(&n),
                "rate={rate} n={n}"
            );
        }
    }

    #[test]
    fn higher_rates_want_more_bytes_buffered_per_transfer() {
        let low = buffer_len(20_000, 512);
        let high = buffer_len(48_000_000, 512);
        assert!(high > low);
    }
}

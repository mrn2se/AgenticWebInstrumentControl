//! The shared Cypress EZ-USB FX2 firmware uploader (SPECIFICATION.md §2.4),
//! used by both this crate's `fx2lafw` and `fx2scope` drivers.
//!
//! Protocol reference: SPECIFICATION.md §2.4 itself, cross-checked while
//! reading (not copying — see this crate's top-level licensing note)
//! `libsigrok`'s `src/ezusb.c` (`ezusb_reset`, `ezusb_install_firmware`),
//! which independently confirms:
//!
//! 1. Halt the 8051: vendor OUT, `bRequest 0xA0`, `wValue 0xE600` (the FX2's
//!    `CPUCS` register), 1 byte of data, `0x01` to assert reset.
//! 2. Upload the image in chunks (`libsigrok` uses 4096 bytes; the actual
//!    limit is the 64 KiB `wValue` can address, not the chunk size) via
//!    vendor OUT, `bRequest 0xA0`, `wValue` = the chunk's load address.
//! 3. Release reset: same request, `wValue 0xE600`, 1 byte, `0x00`.
//! 4. The device disconnects and re-enumerates running the uploaded image.
//!    [`wait_for_reenumeration`] is how callers find it again — keyed on
//!    the USB bus path, per SPECIFICATION.md §2.3, never the device number
//!    (which is guaranteed to change) or VID:PID alone (which changes for
//!    some devices and not others — see SPECIFICATION.md §5.1).

use std::time::Duration;

use async_trait::async_trait;
use nusb::transfer::{ControlOut, ControlType, Recipient};
use nusb::DeviceInfo;

use crate::error::{NativeError, Result};
use crate::firmware::{Firmware, MAX_CHUNK_SIZE};

/// The FX2's `CPUCS` (CPU Control & Status) register address. Vendor request
/// `0xA0` writes directly to on-chip Cypress firmware-load memory, and
/// `0xE600` is the fixed address of this particular register on every FX2 —
/// it is not something the uploaded firmware image chooses.
const CPUCS_ADDRESS: u16 = 0xE600;
/// Cypress's own vendor command for "load a byte range of on-chip RAM /
/// registers starting at `wValue`", used for both the CPUCS poke and the
/// firmware image itself.
const ANCHOR_LOAD_REQUEST: u8 = 0xA0;

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// How often to re-poll `nusb::list_devices()` while waiting for a device to
/// come back after a firmware upload.
const REENUMERATION_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// SPECIFICATION.md §4.5: "Settle time 2-4 s." Generous headroom beyond
/// that for a busy hub or a slow host controller.
pub const DEFAULT_REENUMERATION_TIMEOUT: Duration = Duration::from_secs(8);

/// The handful of USB operations firmware upload needs, abstracted so the
/// exact request sequence in [`upload`] can be exercised in a unit test
/// against a mock, with no hardware and no `nusb` device involved.
///
/// Real callers use the blanket impl on `&nusb::Device`,
/// [`ControlTransport for &nusb::Device`](#impl-ControlTransport-for-%26Device).
#[async_trait]
pub trait ControlTransport: Send + Sync {
    /// A vendor, device-recipient control OUT transfer — the shape every
    /// FX2 upload request uses.
    async fn vendor_write(&self, request: u8, value: u16, data: &[u8]) -> Result<()>;
}

#[async_trait]
impl ControlTransport for nusb::Device {
    async fn vendor_write(&self, request: u8, value: u16, data: &[u8]) -> Result<()> {
        self.control_out(
            ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request,
                value,
                index: 0,
                data,
            },
            CONTROL_TIMEOUT,
        )
        .await?;
        Ok(())
    }
}

/// Runs the full SPECIFICATION.md §2.4 sequence against `transport`: halt,
/// upload every chunk of `firmware` in ascending address order, release.
///
/// Does **not** wait for re-enumeration — that needs the device's bus path,
/// which the caller already has (it is what identified this device in the
/// first place) and this function has no reason to know about. Call
/// [`wait_for_reenumeration`] next.
pub async fn upload<T: ControlTransport>(transport: &T, firmware: &Firmware) -> Result<()> {
    tracing::debug!(bytes = firmware.len(), "halting 8051 (CPUCS <- 0x01)");
    transport
        .vendor_write(ANCHOR_LOAD_REQUEST, CPUCS_ADDRESS, &[0x01])
        .await?;

    for (address, chunk) in firmware.chunks(MAX_CHUNK_SIZE) {
        tracing::trace!(address, len = chunk.len(), "uploading firmware chunk");
        transport
            .vendor_write(ANCHOR_LOAD_REQUEST, address, chunk)
            .await?;
    }

    tracing::debug!("releasing 8051 reset (CPUCS <- 0x00)");
    transport
        .vendor_write(ANCHOR_LOAD_REQUEST, CPUCS_ADDRESS, &[0x00])
        .await?;

    Ok(())
}

/// Finds the device currently at `bus`/`port_path`, if any is enumerated
/// there right now. `pub(crate)` so `fx2lafw::driver` can locate a device
/// before it has been opened, e.g. right before firmware upload.
pub(crate) async fn find_at_path(bus: u8, port_path: &[u8]) -> Result<Option<DeviceInfo>> {
    let devices = list_devices().await?;
    Ok(devices
        .into_iter()
        .find(|d| d.busnum() == bus && d.port_chain() == port_path))
}

/// `nusb::list_devices()`, run on the blocking pool.
///
/// On Linux it reads sysfs synchronously, and a read under a hub stalls for
/// as long as the kernel holds that hub's lock — which it does for the whole
/// ~90 s it spends retrying a sibling port whose device won't enumerate
/// (`device descriptor read/64, error -110`). Observed on the bench: a
/// misbehaving RP2040 on port 1 of the scope's hub froze the async worker
/// polling for the scope's re-enumeration, so no timer — the 8 s
/// re-enumeration timeout included — could fire until the kernel gave up.
pub async fn list_devices() -> Result<Vec<DeviceInfo>> {
    use nusb::MaybeFuture;
    tokio::task::spawn_blocking(|| nusb::list_devices().wait().map(Iterator::collect))
        .await
        .expect("usb enumeration task panicked")
        .map_err(NativeError::from)
}

/// Whether a freshly-polled sighting of the device is evidence that it has
/// actually re-enumerated (as opposed to just being the same, still-running
/// enumeration we started with). Split out from [`wait_for_reenumeration`]
/// so this decision — the actual "did it come back" logic — is unit
/// testable without touching real hardware.
///
/// Accepts either signal, since a poll can miss a very short disconnect
/// window:
/// - we have, at some point since starting, observed the device *absent*
///   at this bus path (a real disconnect happened), or
/// - the device address changed from the one we started with (a new
///   enumeration got a new address even if we never caught it being gone).
fn is_fresh_enumeration(baseline_addr: Option<u8>, seen_absent: bool, candidate_addr: u8) -> bool {
    seen_absent || baseline_addr != Some(candidate_addr)
}

/// How long to keep retrying an open refused with `PermissionDenied`.
const PERMISSION_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);
const PERMISSION_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Opens `info`, retrying briefly while the device node is not yet ours.
///
/// The kernel announces a new device — the netlink event the hot-plug watcher
/// hears, and the moment `list_devices` sees it — *before* udev has applied
/// the rules that give `plugdev` access to its node. Running as root that
/// window is invisible; a rootless container falls into it and gets EACCES.
/// Observed on the bench: powering a port back on under `make run` failed
/// with "permission denied" in the same millisecond the device appeared. Any
/// other error is returned at once; a node that stays unreadable past the
/// settle time is a real permission problem and is reported as such.
pub async fn open_device(
    info: &nusb::DeviceInfo,
) -> std::result::Result<nusb::Device, nusb::Error> {
    let deadline = tokio::time::Instant::now() + PERMISSION_SETTLE;
    loop {
        match info.open().await {
            Err(e)
                if e.kind() == nusb::ErrorKind::PermissionDenied
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(PERMISSION_POLL).await;
            }
            other => return other,
        }
    }
}

/// Waits for the device at `bus`/`port_path` to disconnect and re-enumerate
/// after [`upload`]'s reset release (SPECIFICATION.md §2.4 step 4, §4.5).
///
/// Keyed on bus path alone — deliberately blind to VID:PID, because some FX2
/// boards keep the same VID:PID across the upload (their EEPROM fixes it)
/// while others change it (SPECIFICATION.md §5.1's oscilloscope), and blind to the
/// device number, which changes across *every* re-enumeration regardless.
pub async fn wait_for_reenumeration(
    bus: u8,
    port_path: &[u8],
    timeout: Duration,
) -> Result<DeviceInfo> {
    // The deadline covers each poll too: one that stalls in the kernel (see
    // [`list_devices`]) runs out the clock rather than outliving it.
    tokio::time::timeout(timeout, poll_for_reenumeration(bus, port_path))
        .await
        .unwrap_or_else(|_| {
            Err(NativeError::ReenumerationTimeout {
                bus,
                port_path: port_path.to_vec(),
                timeout_ms: timeout.as_millis() as u64,
            })
        })
}

async fn poll_for_reenumeration(bus: u8, port_path: &[u8]) -> Result<DeviceInfo> {
    let baseline_addr = find_at_path(bus, port_path)
        .await?
        .map(|d| d.device_address());
    let mut seen_absent = false;

    loop {
        match find_at_path(bus, port_path).await? {
            None => {
                seen_absent = true;
            }
            Some(info) => {
                if is_fresh_enumeration(baseline_addr, seen_absent, info.device_address()) {
                    tracing::info!(
                        bus,
                        port_path = ?port_path,
                        vid = format_args!("{:04x}", info.vendor_id()),
                        pid = format_args!("{:04x}", info.product_id()),
                        "device re-enumerated"
                    );
                    return Ok(info);
                }
            }
        }
        tokio::time::sleep(REENUMERATION_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every `vendor_write` call so the exact request sequence can
    /// be asserted against, with no real USB device involved.
    #[derive(Default)]
    struct MockTransport {
        calls: Mutex<Vec<(u8, u16, Vec<u8>)>>,
    }

    #[async_trait]
    impl ControlTransport for MockTransport {
        async fn vendor_write(&self, request: u8, value: u16, data: &[u8]) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((request, value, data.to_vec()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn upload_sequence_matches_spec_2_4() {
        let transport = MockTransport::default();
        let firmware = Firmware::from_bytes("x.fw", vec![0xAB; 5000]).unwrap();

        upload(&transport, &firmware).await.unwrap();

        let calls = transport.calls.lock().unwrap();
        // 1 halt + 2 chunks (4096 + 904) + 1 release = 4 requests.
        assert_eq!(calls.len(), 4);

        // Step 1: halt.
        assert_eq!(calls[0].0, ANCHOR_LOAD_REQUEST);
        assert_eq!(calls[0].1, CPUCS_ADDRESS);
        assert_eq!(calls[0].2, vec![0x01]);

        // Step 2: chunks, ascending address, every byte accounted for.
        assert_eq!(calls[1].0, ANCHOR_LOAD_REQUEST);
        assert_eq!(calls[1].1, 0);
        assert_eq!(calls[1].2.len(), 4096);
        assert!(calls[1].2.iter().all(|&b| b == 0xAB));

        assert_eq!(calls[2].0, ANCHOR_LOAD_REQUEST);
        assert_eq!(calls[2].1, 4096);
        assert_eq!(calls[2].2.len(), 5000 - 4096);

        // Step 3: release, only after every chunk has gone out.
        assert_eq!(calls[3].0, ANCHOR_LOAD_REQUEST);
        assert_eq!(calls[3].1, CPUCS_ADDRESS);
        assert_eq!(calls[3].2, vec![0x00]);
    }

    #[tokio::test]
    async fn upload_of_an_image_smaller_than_one_chunk_still_halts_and_releases() {
        let transport = MockTransport::default();
        let firmware = Firmware::from_bytes("x.fw", vec![0x00; 100]).unwrap();

        upload(&transport, &firmware).await.unwrap();

        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "halt + one chunk + release");
        assert_eq!(calls[1].2.len(), 100);
    }

    #[tokio::test]
    async fn upload_never_writes_to_cpucs_between_halt_and_release() {
        let transport = MockTransport::default();
        // A firmware image that happens to contain bytes equal to the CPUCS
        // address is irrelevant: the *wValue* (load address) must never be
        // CPUCS except for the halt/release requests themselves.
        let firmware = Firmware::from_bytes("x.fw", vec![0xE6, 0x00, 0x01, 0x02]).unwrap();
        upload(&transport, &firmware).await.unwrap();

        let calls = transport.calls.lock().unwrap();
        let middle = &calls[1..calls.len() - 1];
        assert!(
            middle.iter().all(|(_, value, _)| *value != CPUCS_ADDRESS),
            "no chunk write may target the CPUCS address"
        );
    }

    #[test]
    fn accepts_reenumeration_after_an_observed_absence() {
        assert!(is_fresh_enumeration(Some(5), true, 5));
        assert!(is_fresh_enumeration(Some(5), true, 6));
    }

    #[test]
    fn accepts_reenumeration_via_address_change_even_without_seeing_absence() {
        assert!(is_fresh_enumeration(Some(5), false, 6));
    }

    #[test]
    fn rejects_the_same_still_running_enumeration() {
        assert!(!is_fresh_enumeration(Some(5), false, 5));
    }

    #[test]
    fn no_baseline_means_any_sighting_counts_as_fresh() {
        // The device wasn't there at all when we started waiting (e.g. we
        // only knew its bus path from config) — first sighting wins.
        assert!(is_fresh_enumeration(None, false, 1));
    }

    #[tokio::test]
    async fn reenumeration_wait_times_out_when_nothing_matches() {
        // Bus 253 is never a real bus in this workspace (see awic-fake's
        // FAKE_BUS), so this can run with no hardware and no risk of ever
        // finding a real match.
        let err = wait_for_reenumeration(253, &[9, 9], Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            NativeError::ReenumerationTimeout { bus: 253, .. }
        ));
    }
}

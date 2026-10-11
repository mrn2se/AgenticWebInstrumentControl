//! Kernel uevent netlink watcher (SPECIFICATION.md §4.3).
//!
//! Opens a raw `AF_NETLINK` / `NETLINK_KOBJECT_UEVENT` socket, joined to
//! multicast group 1 (`kernel` uevents — not `udevd`'s own re-broadcast
//! group). This is the kernel's own uevent feed with **no** `libudev`
//! framing on top: no `udev` crate, no C dependency, matching this project's
//! "pure Rust" constraint. `libc` supplies the raw syscalls.
//!
//! # The network-namespace trap (SPECIFICATION.md §4.3, §1.3)
//!
//! This socket is scoped by **network namespace**, not by any USB or device
//! namespace. A process in its own netns — which is the *default* for a
//! container runtime, `podman run` included — binds and joins the multicast
//! group successfully, receives no error of any kind, and then simply never
//! gets a message: the kernel delivers uevents only within the netns that
//! was active when the device event occurred (normally the host's init
//! netns). There is no way to detect this failure mode from inside the
//! socket API itself — it looks identical to "nothing has been plugged in
//! yet". The fix is `--net=host` on the container (§1.3); the fallback for
//! when that isn't available is [`super::polling::PollingWatcher`].
//!
//! Message parsing ([`parse_uevent`]) is a pure function from bytes to
//! `Option<UsbEvent>` so it is unit-testable without a socket, a kernel, or
//! any hardware.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::str::FromStr;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;

use awic_core::DeviceId;

use super::{DeviceWatcher, UsbEvent};

/// Kernel multicast group carrying uevents (as opposed to group 2, which
/// carries `udevd`'s post-processed re-broadcast — irrelevant here since
/// this crate talks to the kernel socket directly, never to `udevd`).
const UEVENT_GROUP: u32 = 1;

/// Large enough for any single kobject uevent message; the kernel does not
/// fragment these across multiple netlink messages.
const RECV_BUFFER_LEN: usize = 8192;

pub struct NetlinkWatcher;

impl NetlinkWatcher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetlinkWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceWatcher for NetlinkWatcher {
    fn spawn(self) -> mpsc::UnboundedReceiver<UsbEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            if let Err(e) = run(tx).await {
                tracing::error!(error = %e, "netlink watcher stopped");
            }
        });
        rx
    }
}

async fn run(tx: mpsc::UnboundedSender<UsbEvent>) -> io::Result<()> {
    let socket = open_socket()?;
    let async_fd = AsyncFd::new(socket)?;
    let mut buf = [0u8; RECV_BUFFER_LEN];

    loop {
        let n = async_fd
            .async_io(Interest::READABLE, |fd| {
                // SAFETY: `fd` is a valid, open netlink socket for the
                // lifetime of this call, and `buf` is sized for the write.
                let ret = unsafe {
                    libc::recv(
                        fd.as_raw_fd(),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                        0,
                    )
                };
                if ret < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            })
            .await?;

        if let Some(event) = parse_uevent(&buf[..n]) {
            if tx.send(event).is_err() {
                // Nobody is listening any more; stop polling the socket.
                return Ok(());
            }
        }
    }
}

fn open_socket() -> io::Result<OwnedFd> {
    // SAFETY: standard two-arg-plus-protocol `socket(2)`; the return value is
    // checked before being trusted as a valid fd.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just returned by `socket(2)` above, is open, and is
    // not owned by anything else yet.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: `sockaddr_nl` is a plain-old-data struct; zero is a valid
    // value for every field (family gets set explicitly below).
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = 0; // let the kernel assign our port id
    addr.nl_groups = UEVENT_GROUP;

    // SAFETY: `addr` is a valid `sockaddr_nl` for the duration of the call,
    // matching the `sockaddr_nl` size passed alongside it.
    let ret = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(fd)
}

/// Parses one kernel uevent message into a [`UsbEvent`], or `None` if it
/// isn't a USB *device*-level add/remove (composite-device interface
/// sub-events, other subsystems entirely, and any action other than
/// `add`/`remove` are all filtered out here).
///
/// Wire format (see `kobject_uevent_env` in the kernel; this is deliberately
/// *not* `libudev`'s wire format, which prefixes an 8-byte "libudev" magic
/// header — that framing exists only on `udevd`'s own re-broadcast socket,
/// group 2, which this watcher does not use):
///
/// ```text
/// "<action>@<devpath>\0KEY=VALUE\0KEY=VALUE\0...\0"
/// ```
pub fn parse_uevent(buf: &[u8]) -> Option<UsbEvent> {
    let mut parts = buf.split(|&b| b == 0).filter(|s| !s.is_empty());

    let header = std::str::from_utf8(parts.next()?).ok()?;
    let (action, header_devpath) = header.split_once('@')?;

    let mut fields: HashMap<&str, &str> = HashMap::new();
    for part in parts {
        if let Ok(s) = std::str::from_utf8(part) {
            if let Some((k, v)) = s.split_once('=') {
                fields.insert(k, v);
            }
        }
    }

    if fields.get("SUBSYSTEM").copied() != Some("usb") {
        return None;
    }
    // Composite devices emit one uevent per interface too
    // (`DEVTYPE=usb_interface`); only the whole-device event carries stable
    // bus-path identity we want to key on.
    if fields.get("DEVTYPE").copied() != Some("usb_device") {
        return None;
    }

    let devpath = fields.get("DEVPATH").copied().unwrap_or(header_devpath);
    let bus_path = devpath.rsplit('/').next()?;
    let device_id = DeviceId::from_str(bus_path).ok()?;

    match action {
        "add" => {
            let (vid, pid) = parse_product(fields.get("PRODUCT")?)?;
            Some(UsbEvent::Added(device_id.with_vid_pid(vid, pid)))
        }
        "remove" => Some(UsbEvent::Removed(device_id)),
        _ => None,
    }
}

/// Parses the kernel's `PRODUCT=<vid>/<pid>/<bcdDevice>` uevent field —
/// lower-case hex, no leading zeros.
fn parse_product(s: &str) -> Option<(u16, u16)> {
    let mut parts = s.split('/');
    let vid = u16::from_str_radix(parts.next()?, 16).ok()?;
    let pid = u16::from_str_radix(parts.next()?, 16).ok()?;
    Some((vid, pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a raw kernel uevent buffer from an action/devpath header and a
    /// list of `KEY=VALUE` fields, exactly as the kernel emits it: NUL
    /// between fields, no trailing NUL required.
    fn uevent(action_at_devpath: &str, fields: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(action_at_devpath.as_bytes());
        buf.push(0);
        for (k, v) in fields {
            buf.extend_from_slice(format!("{k}={v}").as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn parses_a_real_looking_add_event() {
        let buf = uevent(
            "add@/devices/pci0000:00/0000:00:14.0/usb3/3-3/3-3.4/3-3.4.2",
            &[
                ("ACTION", "add"),
                (
                    "DEVPATH",
                    "/devices/pci0000:00/0000:00:14.0/usb3/3-3/3-3.4/3-3.4.2",
                ),
                ("SUBSYSTEM", "usb"),
                ("DEVTYPE", "usb_device"),
                ("PRODUCT", "4b4/602a/100"),
                ("BUSNUM", "003"),
                ("DEVNUM", "013"),
            ],
        );
        let event = parse_uevent(&buf).expect("should parse");
        assert_eq!(
            event,
            UsbEvent::Added(DeviceId::new(3, vec![3, 4, 2], 0x04b4, 0x602a))
        );
    }

    #[test]
    fn parses_a_remove_event_with_no_product_field() {
        // The kernel's "remove" uevents for a device that's already gone
        // don't necessarily carry PRODUCT; removal must not depend on it.
        let buf = uevent(
            "remove@/devices/pci0000:00/0000:00:14.0/usb3/3-3/3-3.4/3-3.4.2",
            &[
                ("ACTION", "remove"),
                ("SUBSYSTEM", "usb"),
                ("DEVTYPE", "usb_device"),
            ],
        );
        let event = parse_uevent(&buf).expect("should parse");
        assert_eq!(
            event,
            UsbEvent::Removed(DeviceId::new(3, vec![3, 4, 2], 0, 0))
        );
    }

    #[test]
    fn ignores_non_usb_subsystems() {
        let buf = uevent(
            "add@/devices/pci0000:00/0000:00:14.0",
            &[("ACTION", "add"), ("SUBSYSTEM", "pci"), ("DEVTYPE", "pci")],
        );
        assert_eq!(parse_uevent(&buf), None);
    }

    #[test]
    fn ignores_per_interface_events_of_a_composite_device() {
        let buf = uevent(
            "add@/devices/.../usb3/3-3/3-3.4.2/3-3.4.2:1.0",
            &[
                ("ACTION", "add"),
                ("SUBSYSTEM", "usb"),
                ("DEVTYPE", "usb_interface"),
                ("PRODUCT", "4b4/602a/100"),
            ],
        );
        assert_eq!(parse_uevent(&buf), None);
    }

    #[test]
    fn ignores_bind_and_unbind_actions() {
        let buf = uevent(
            "bind@/devices/.../usb3/3-3/3-3.4.2",
            &[
                ("ACTION", "bind"),
                ("SUBSYSTEM", "usb"),
                ("DEVTYPE", "usb_device"),
                ("PRODUCT", "4b4/602a/100"),
            ],
        );
        assert_eq!(parse_uevent(&buf), None);
    }

    #[test]
    fn malformed_buffer_is_ignored_not_panicking() {
        assert_eq!(parse_uevent(&[]), None);
        assert_eq!(parse_uevent(b"garbage-no-at-sign\0"), None);
        assert_eq!(parse_uevent(&[0xff, 0xfe, 0x00]), None);
    }

    #[test]
    fn falls_back_to_header_devpath_when_no_devpath_field() {
        let buf = uevent(
            "add@/devices/pci0000:00/usb3/3-3",
            &[
                ("ACTION", "add"),
                ("SUBSYSTEM", "usb"),
                ("DEVTYPE", "usb_device"),
                ("PRODUCT", "2109/2822/100"),
            ],
        );
        let event = parse_uevent(&buf).expect("should parse");
        assert_eq!(
            event,
            UsbEvent::Added(DeviceId::new(3, vec![3], 0x2109, 0x2822))
        );
    }
}

//! `serial` terminal source (SPECIFICATION.md §11.2.1): discovers USB-serial
//! interfaces from sysfs, opens their tty node in raw mode with `rustix`,
//! and drives it with `tokio::io::unix::AsyncFd` — reopening automatically
//! when the same interface path reappears after its device left the bus.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rustix::fs::{Mode, OFlags};
use rustix::termios::{ControlModes, OptionalActions};
use tokio::io::unix::AsyncFd;

use awic_proto::{Parity, SerialSettings, StopBits, TerminalKind, TerminalSourceInfo};

use super::session::{
    Connected, OpenSettings, SourceConnection, SourceEvent, TerminalError, TerminalSourceProvider,
};

/// How long a fresh `term.open` retries an `EACCES` on the tty node before
/// giving up — udev grants group access to a just-appeared node slightly
/// after the kernel announces it, the same race `awic-usb` already handles
/// for `/dev/bus/usb` nodes.
const EACCES_RETRY_TOTAL: Duration = Duration::from_millis(1000);
const EACCES_RETRY_STEP: Duration = Duration::from_millis(50);

/// How often a `waiting` session checks whether its interface has come back.
const WAITING_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One USB-serial interface found under sysfs, before it's turned into a
/// [`TerminalSourceInfo`] (which additionally needs an availability check —
/// kept separate so [`discover`] itself needs no filesystem access beyond
/// `sysfs_root`, and is unit-testable against a fake tree with no tty nodes
/// at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSerial {
    /// The interface's own directory name under
    /// `<sysfs_root>/bus/usb/devices/`, e.g. `"3-3.4.4:1.1"` — the USB bus
    /// path plus `:<config>.<interface>`. Used verbatim as the source id's
    /// suffix (`serial:3-3.4.4:1.1`).
    pub interface_path: String,
    pub tty_name: String,
    /// The device's USB product string, from its parent device node's
    /// sysfs `product` attribute (interfaces don't have their own).
    pub product: Option<String>,
}

/// Scans `<sysfs_root>/bus/usb/devices/*` for USB-serial interfaces:
/// CDC-ACM's `<intf>/tty/ttyACM*` and usb-serial's `<intf>/ttyUSB*`
/// (SPEC §11.2.1). Returns nothing, rather than erroring, if `sysfs_root`
/// doesn't have the expected shape — a missing sysfs tree means no serial
/// sources, not a crash.
pub fn discover(sysfs_root: &Path) -> Vec<DiscoveredSerial> {
    let devices_dir = sysfs_root.join("bus/usb/devices");
    let Ok(entries) = std::fs::read_dir(&devices_dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let interface_path = entry.file_name().to_string_lossy().into_owned();
        // Interface directories are named "<bus-port-path>:<config>.<intf>",
        // e.g. "3-3.4.4:1.1" — a device's own directory ("3-3.4.4") has no
        // colon, and is not itself an interface.
        if !interface_path.contains(':') {
            continue;
        }
        let path = entry.path();

        // CDC-ACM (e.g. the Raspberry Pi Debug Probe's UART): a nested `tty`
        // subdirectory holding the actual ttyACM* entry.
        if let Ok(tty_entries) = std::fs::read_dir(path.join("tty")) {
            for tty in tty_entries.flatten() {
                let tty_name = tty.file_name().to_string_lossy().into_owned();
                out.push(build(sysfs_root, &interface_path, tty_name));
            }
        }
        // usb-serial bridges (FTDI, CH340, CP210x, PL2303): the ttyUSB*
        // entry sits directly under the interface, no `tty` subdirectory.
        if let Ok(direct) = std::fs::read_dir(&path) {
            for d in direct.flatten() {
                let name = d.file_name().to_string_lossy().into_owned();
                if name.starts_with("ttyUSB") {
                    out.push(build(sysfs_root, &interface_path, name));
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.interface_path, &a.tty_name).cmp(&(&b.interface_path, &b.tty_name)));
    out
}

/// The device's own bus path, without the interface's ":<config>.<intf>"
/// suffix, e.g. `"3-3.4.4:1.1"` -> `"3-3.4.4"` — that's where sysfs puts the
/// USB product string (interfaces don't have their own), and it's also
/// exactly what `TerminalSourceInfo::device` reports for the USB tree
/// module's terminal icon (SPEC §11.2.1).
fn device_path_of(interface_path: &str) -> &str {
    interface_path.split(':').next().unwrap_or(interface_path)
}

fn build(sysfs_root: &Path, interface_path: &str, tty_name: String) -> DiscoveredSerial {
    let device_path = device_path_of(interface_path);
    let product = std::fs::read_to_string(
        sysfs_root
            .join("bus/usb/devices")
            .join(device_path)
            .join("product"),
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    DiscoveredSerial {
        interface_path: interface_path.to_string(),
        tty_name,
        product,
    }
}

fn label(d: &DiscoveredSerial) -> String {
    match &d.product {
        Some(product) => format!("{} — {product}", d.tty_name),
        None => d.tty_name.clone(),
    }
}

/// Whether `tty_dir/tty_name` can be opened right now, and if not, why —
/// distinguishing "not there yet" from "there but not accessible yet" so
/// the UI's disabled reason is actually informative (SPEC §11.2.1).
fn availability(tty_dir: &Path, tty_name: &str) -> (bool, Option<String>) {
    let path = tty_dir.join(tty_name);
    if !path.exists() {
        return (false, Some("tty node not present".to_string()));
    }
    match rustix::fs::access(
        &path,
        rustix::fs::Access::READ_OK | rustix::fs::Access::WRITE_OK,
    ) {
        Ok(()) => (true, None),
        Err(_) => (
            false,
            Some("permission denied (waiting for udev to grant access)".to_string()),
        ),
    }
}

/// The `serial` [`TerminalSourceProvider`]: not persistent — the last
/// connection leaving a serial session closes the port (SPEC §11.2.1).
pub struct SerialSourceProvider {
    sysfs_root: PathBuf,
    tty_dir: PathBuf,
}

impl SerialSourceProvider {
    pub fn new(sysfs_root: impl Into<PathBuf>, tty_dir: impl Into<PathBuf>) -> Self {
        Self {
            sysfs_root: sysfs_root.into(),
            tty_dir: tty_dir.into(),
        }
    }
}

#[async_trait]
impl TerminalSourceProvider for SerialSourceProvider {
    fn kind(&self) -> TerminalKind {
        TerminalKind::Serial
    }

    fn persistent(&self) -> bool {
        false
    }

    fn list(&self) -> Vec<TerminalSourceInfo> {
        discover(&self.sysfs_root)
            .into_iter()
            .map(|d| {
                let (available, unavailable_reason) = availability(&self.tty_dir, &d.tty_name);
                TerminalSourceInfo {
                    source: format!("serial:{}", d.interface_path),
                    kind: TerminalKind::Serial,
                    label: label(&d),
                    // The device's own bus path, without the ":<config>.<intf>"
                    // suffix `source` carries — the join key the USB tree
                    // module uses to place a terminal icon on the right port
                    // (SPEC §11.2.1) without parsing `source`'s structure.
                    device: Some(device_path_of(&d.interface_path).to_string()),
                    available,
                    unavailable_reason,
                }
            })
            .collect()
    }

    async fn connect(
        &self,
        source_id: &str,
        settings: OpenSettings,
    ) -> Result<Connected, TerminalError> {
        let interface_path = source_id
            .strip_prefix("serial:")
            .ok_or_else(|| TerminalError::UnknownSource(source_id.to_string()))?;
        let settings = settings
            .serial
            .ok_or_else(|| TerminalError::SettingsRequired(source_id.to_string()))?;

        let discovered = discover(&self.sysfs_root)
            .into_iter()
            .find(|d| d.interface_path == interface_path)
            .ok_or_else(|| TerminalError::UnknownSource(source_id.to_string()))?;

        let tty_path = self.tty_dir.join(&discovered.tty_name);
        let fd = open_with_eacces_retry(&tty_path, &settings)
            .await
            .map_err(|err| open_error(&discovered.tty_name, &err))?;

        let sysfs_interface_dir = self.sysfs_root.join("bus/usb/devices").join(interface_path);
        Ok(Connected {
            connection: Box::new(SerialConnection {
                tty_path,
                sysfs_interface_dir,
                settings,
                fd: Some(fd),
            }),
            initial_backlog: Vec::new(),
            label: label(&discovered),
        })
    }
}

/// What a failed open tells the caller. A port another program holds (it
/// answers `EBUSY` to our `TIOCEXCL`-style exclusive open, as when `luaexec`
/// has an ESP32's port) used to come back as `opening /host/dev/ttyACM2:
/// Device or resource busy (os error 16)` — the container's path, and no word
/// of who holds it. Named by its tty, not the path inside the container.
fn open_error(tty_name: &str, err: &std::io::Error) -> TerminalError {
    if err.raw_os_error() == Some(libc::EBUSY) {
        TerminalError::Busy(tty_name.to_string())
    } else {
        TerminalError::Io(format!("opening {tty_name}: {err}"))
    }
}

/// Opens and configures the tty at `path`, retrying a permission error for
/// up to [`EACCES_RETRY_TOTAL`] — see the module doc.
async fn open_with_eacces_retry(
    path: &Path,
    settings: &SerialSettings,
) -> std::io::Result<AsyncFd<OwnedFd>> {
    let deadline = tokio::time::Instant::now() + EACCES_RETRY_TOTAL;
    loop {
        match open_and_configure(path, settings) {
            Ok(fd) => return Ok(fd),
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(EACCES_RETRY_STEP).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Opens `path` `O_RDWR|O_NOCTTY|O_NONBLOCK|O_CLOEXEC`, claims it exclusively
/// (`TIOCEXCL`), and applies `settings` as a raw termios (SPEC §11.2.1).
fn open_and_configure(path: &Path, settings: &SerialSettings) -> std::io::Result<AsyncFd<OwnedFd>> {
    let flags = OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd: OwnedFd = rustix::fs::open(path, flags, Mode::empty())?;

    // TIOCEXCL has no rustix wrapper; a single raw ioctl via `libc` (already
    // a workspace dependency, e.g. `awic-usb`) rather than pulling in a
    // dedicated crate for one call. SAFETY: `fd` is a tty we just opened
    // ourselves, and TIOCEXCL takes no argument.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCEXCL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    configure_termios(&fd, settings)?;
    // SAFETY: `fd` is an `OwnedFd`: the fd it holds stays open and unchanged
    // until the `AsyncFd` drops it.
    Ok(unsafe { AsyncFd::register(fd) }?)
}

fn configure_termios(fd: &OwnedFd, settings: &SerialSettings) -> std::io::Result<()> {
    let mut termios = rustix::termios::tcgetattr(fd)?;
    termios.make_raw();
    termios.control_modes |= ControlModes::CLOCAL | ControlModes::CREAD;
    termios.control_modes -= ControlModes::CSIZE;
    termios.control_modes |= match settings.data_bits {
        5 => ControlModes::CS5,
        6 => ControlModes::CS6,
        7 => ControlModes::CS7,
        _ => ControlModes::CS8,
    };
    match settings.parity {
        Parity::None => termios.control_modes -= ControlModes::PARENB | ControlModes::PARODD,
        Parity::Even => {
            termios.control_modes |= ControlModes::PARENB;
            termios.control_modes -= ControlModes::PARODD;
        }
        Parity::Odd => termios.control_modes |= ControlModes::PARENB | ControlModes::PARODD,
    }
    match settings.stop_bits {
        StopBits::One => termios.control_modes -= ControlModes::CSTOPB,
        StopBits::Two => termios.control_modes |= ControlModes::CSTOPB,
    }
    termios
        .set_speed(settings.baud)
        .map_err(std::io::Error::from)?;
    rustix::termios::tcsetattr(fd, OptionalActions::Now, &termios).map_err(std::io::Error::from)?;
    Ok(())
}

async fn read_once(async_fd: &AsyncFd<OwnedFd>, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut guard = async_fd.readable().await?;
        match guard.try_io(|inner| {
            rustix::io::read(inner.get_ref(), &mut *buf).map_err(std::io::Error::from)
        }) {
            Ok(result) => return result,
            Err(_would_block) => continue,
        }
    }
}

async fn write_all(async_fd: &AsyncFd<OwnedFd>, data: &[u8]) -> std::io::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let mut guard = async_fd.writable().await?;
        match guard.try_io(|inner| {
            rustix::io::write(inner.get_ref(), &data[offset..]).map_err(std::io::Error::from)
        }) {
            Ok(Ok(n)) => offset += n,
            Ok(Err(err)) => return Err(err),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// One open serial port. `fd` is `None` while `Waiting` — the device left
/// the bus and [`SourceConnection::recv`] is polling `sysfs_interface_dir`
/// for the same interface to reappear, at which point it reopens `tty_path`
/// with the original `settings` and reports [`SourceEvent::Resumed`].
struct SerialConnection {
    tty_path: PathBuf,
    sysfs_interface_dir: PathBuf,
    settings: SerialSettings,
    fd: Option<AsyncFd<OwnedFd>>,
}

#[async_trait]
impl SourceConnection for SerialConnection {
    async fn recv(&mut self) -> SourceEvent {
        match &self.fd {
            Some(async_fd) => {
                let mut buf = [0u8; 4096];
                match read_once(async_fd, &mut buf).await {
                    Ok(0) => {
                        // EOF on a tty normally means the device is gone.
                        self.fd = None;
                        SourceEvent::Waiting("device disconnected".to_string())
                    }
                    Ok(n) => SourceEvent::Data(buf[..n].to_vec()),
                    Err(err) => {
                        tracing::info!(
                            tty = %self.tty_path.display(),
                            %err,
                            "serial read failed; treating as the device having left the bus"
                        );
                        self.fd = None;
                        SourceEvent::Waiting(format!("device disconnected: {err}"))
                    }
                }
            }
            // Waiting: poll for the interface to reappear in sysfs, then
            // reopen the tty with the original settings.
            None => loop {
                if self.sysfs_interface_dir.exists() {
                    if let Ok(fd) = open_and_configure(&self.tty_path, &self.settings) {
                        self.fd = Some(fd);
                        return SourceEvent::Resumed;
                    }
                    // Node exists in sysfs but the tty device isn't ready
                    // yet (udev race) — keep polling.
                }
                tokio::time::sleep(WAITING_POLL_INTERVAL).await;
            },
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        match &self.fd {
            Some(async_fd) => write_all(async_fd, data)
                .await
                .map_err(|err| TerminalError::Io(err.to_string())),
            None => Err(TerminalError::Io(
                "device is not present (session is waiting for it to reappear)".to_string(),
            )),
        }
    }
}

/// A fake `<sysfs_root>/bus/usb/devices/...` tree for tests, built in a
/// tempdir — no real hardware or root sysfs access needed.
#[cfg(test)]
pub(crate) struct FakeSysfs {
    pub root: tempfile::TempDir,
}

#[cfg(test)]
impl FakeSysfs {
    pub fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Adds a CDC-ACM-style interface: `<intf>/tty/<tty_name>`, with an
    /// optional `product` string on the parent device directory.
    pub fn add_cdc_acm(&self, interface_path: &str, tty_name: &str, product: Option<&str>) {
        let intf_dir = self.devices_dir().join(interface_path);
        std::fs::create_dir_all(intf_dir.join("tty").join(tty_name)).unwrap();
        self.write_product(interface_path, product);
    }

    /// Adds a usb-serial-bridge-style interface: `<intf>/<tty_name>` directly.
    pub fn add_usb_serial(&self, interface_path: &str, tty_name: &str, product: Option<&str>) {
        let intf_dir = self.devices_dir().join(interface_path);
        std::fs::create_dir_all(intf_dir.join(tty_name)).unwrap();
        self.write_product(interface_path, product);
    }

    pub fn remove_interface(&self, interface_path: &str) {
        let _ = std::fs::remove_dir_all(self.devices_dir().join(interface_path));
    }

    fn write_product(&self, interface_path: &str, product: Option<&str>) {
        if let Some(product) = product {
            let device_path = interface_path.split(':').next().unwrap();
            let device_dir = self.devices_dir().join(device_path);
            std::fs::create_dir_all(&device_dir).unwrap();
            std::fs::write(device_dir.join("product"), format!("{product}\n")).unwrap();
        }
    }

    fn devices_dir(&self) -> PathBuf {
        self.path().join("bus/usb/devices")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_a_cdc_acm_interface_with_its_product_string() {
        let fs = FakeSysfs::new();
        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", Some("Raspberry Pi Debug Probe"));

        let found = discover(fs.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].interface_path, "3-3.4.4:1.1");
        assert_eq!(found[0].tty_name, "ttyACM0");
        assert_eq!(
            found[0].product.as_deref(),
            Some("Raspberry Pi Debug Probe")
        );
    }

    #[test]
    fn discovers_a_usb_serial_bridge_interface_directly_under_the_interface() {
        let fs = FakeSysfs::new();
        fs.add_usb_serial("1-2:1.0", "ttyUSB0", Some("FT232R USB UART"));

        let found = discover(fs.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tty_name, "ttyUSB0");
        assert_eq!(found[0].product.as_deref(), Some("FT232R USB UART"));
    }

    #[test]
    fn discovers_multiple_interfaces_and_ignores_non_interface_entries() {
        let fs = FakeSysfs::new();
        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", Some("Raspberry Pi Debug Probe"));
        fs.add_cdc_acm("3-3.4.1.3:1.0", "ttyACM1", Some("CMSIS-DAP_LU"));
        // A bare device directory (no colon) must not be mistaken for an interface.
        std::fs::create_dir_all(fs.path().join("bus/usb/devices/3-3")).unwrap();

        let found = discover(fs.path());
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|d| d.tty_name == "ttyACM0"));
        assert!(found.iter().any(|d| d.tty_name == "ttyACM1"));
    }

    #[test]
    fn missing_product_file_yields_none_not_an_error() {
        let fs = FakeSysfs::new();
        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", None);
        let found = discover(fs.path());
        assert_eq!(found[0].product, None);
    }

    #[test]
    fn discover_reflects_an_interface_disappearing_and_reappearing() {
        // Mirrors the real lifecycle SPEC §11.2.1 asks for: a serial session
        // whose device left the bus is `waiting` (handled in
        // `SerialConnection::recv`); discovery itself must simply stop, then
        // resume, reporting that interface as the underlying bus events fire.
        let fs = FakeSysfs::new();
        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", Some("Raspberry Pi Debug Probe"));
        assert_eq!(discover(fs.path()).len(), 1);

        fs.remove_interface("3-3.4.4:1.1");
        assert!(
            discover(fs.path()).is_empty(),
            "removed interface must no longer be discovered"
        );

        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", Some("Raspberry Pi Debug Probe"));
        assert_eq!(
            discover(fs.path()).len(),
            1,
            "must be discovered again once it reappears"
        );
    }

    #[test]
    fn missing_sysfs_root_yields_no_sources() {
        let found = discover(Path::new("/does/not/exist"));
        assert!(found.is_empty());
    }

    #[test]
    fn label_combines_tty_name_and_product() {
        let d = DiscoveredSerial {
            interface_path: "3-3.4.4:1.1".into(),
            tty_name: "ttyACM0".into(),
            product: Some("Raspberry Pi Debug Probe".into()),
        };
        assert_eq!(label(&d), "ttyACM0 — Raspberry Pi Debug Probe");

        let d = DiscoveredSerial {
            interface_path: "3-3.4.4:1.1".into(),
            tty_name: "ttyACM0".into(),
            product: None,
        };
        assert_eq!(label(&d), "ttyACM0");
    }

    #[test]
    fn availability_reports_a_specific_reason_when_the_tty_node_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let (available, reason) = availability(dir.path(), "ttyACM0");
        assert!(!available);
        assert_eq!(reason.as_deref(), Some("tty node not present"));
    }

    #[test]
    fn list_reports_source_ids_prefixed_with_serial() {
        let fs = FakeSysfs::new();
        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", Some("Raspberry Pi Debug Probe"));
        let tty_dir = tempfile::tempdir().unwrap(); // no tty node -> unavailable, but still listed
        let provider = SerialSourceProvider::new(fs.path(), tty_dir.path());
        let sources = provider.list();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source, "serial:3-3.4.4:1.1");
        assert_eq!(sources[0].device.as_deref(), Some("3-3.4.4"));
        assert!(!sources[0].available);
    }

    #[test]
    fn a_port_held_elsewhere_is_reported_as_busy_by_its_tty_name() {
        let err = open_error("ttyACM2", &std::io::Error::from_raw_os_error(libc::EBUSY));
        assert!(
            matches!(&err, TerminalError::Busy(name) if name == "ttyACM2"),
            "{err:?}"
        );
        assert!(err.to_string().contains("another program"), "{err}");
        let other = open_error("ttyACM2", &std::io::Error::from_raw_os_error(libc::ENOENT));
        assert!(
            matches!(&other, TerminalError::Io(msg) if msg.starts_with("opening ttyACM2:")),
            "{other:?}"
        );
    }

    #[tokio::test]
    async fn connect_without_settings_is_refused() {
        let fs = FakeSysfs::new();
        fs.add_cdc_acm("3-3.4.4:1.1", "ttyACM0", None);
        let tty_dir = tempfile::tempdir().unwrap();
        let provider = SerialSourceProvider::new(fs.path(), tty_dir.path());
        let Err(err) = provider
            .connect("serial:3-3.4.4:1.1", OpenSettings::default())
            .await
        else {
            panic!("expected an error");
        };
        assert_eq!(
            err,
            TerminalError::SettingsRequired("serial:3-3.4.4:1.1".to_string())
        );
    }

    #[tokio::test]
    async fn connect_to_an_unknown_interface_is_refused() {
        let fs = FakeSysfs::new();
        let tty_dir = tempfile::tempdir().unwrap();
        let provider = SerialSourceProvider::new(fs.path(), tty_dir.path());
        let settings = SerialSettings {
            baud: 9600,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
        };
        let Err(err) = provider
            .connect("serial:does-not-exist:1.0", OpenSettings::serial(settings))
            .await
        else {
            panic!("expected an error");
        };
        assert_eq!(
            err,
            TerminalError::UnknownSource("serial:does-not-exist:1.0".to_string())
        );
    }
}

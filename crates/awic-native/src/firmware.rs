//! FX2 firmware blobs and the resolution chain that finds them
//! (SPECIFICATION.md §13).
//!
//! Protocol reference: the Debian `sigrok-firmware-fx2lafw` package installs
//! `.fw` files under `/usr/share/sigrok-firmware`. Inspecting one of those
//! files directly (`fx2lafw-saleae-logic.fw`, 8120 bytes) confirms
//! SPECIFICATION.md §2.4's claim: it is a raw 8051 code image with no
//! header, no Intel-HEX `:`-record framing, and no length/checksum
//! trailer — just bytes to be written starting at load address `0x0000`.
//! `libsigrok`'s `ezusb_install_firmware` (GPL, read only for this fact, not
//! copied) confirms the same by loading the file as an opaque byte blob and
//! slicing it directly by chunk offset.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{NativeError, Result};

/// The setup packet's `wValue` carries the load address and is 16 bits wide,
/// so no FX2 firmware image can be larger than this (matches the
/// `sr_resource_load(..., 1 << 16)` cap referenced above).
pub const MAX_FIRMWARE_SIZE: usize = 1 << 16;

/// A firmware chunk size that fits in one control transfer while leaving
/// headroom under common host-controller/hub max-control-transfer limits
/// (SPECIFICATION.md §2.4: "≤4096-byte chunks").
pub const MAX_CHUNK_SIZE: usize = 4096;

/// A loaded FX2 firmware image: a flat byte blob loaded from address 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firmware {
    bytes: Vec<u8>,
}

impl Firmware {
    /// Wraps already-loaded bytes, validating the size constraints an FX2
    /// vendor-write upload requires.
    pub fn from_bytes(name: &str, bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            return Err(NativeError::FirmwareEmpty(name.to_string()));
        }
        if bytes.len() > MAX_FIRMWARE_SIZE {
            return Err(NativeError::FirmwareTooLarge {
                name: name.to_string(),
                size: bytes.len(),
            });
        }
        Ok(Self { bytes })
    }

    /// Reads a `.fw` file from disk and validates it.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|source| NativeError::FirmwareIo {
            path: path.display().to_string(),
            source,
        })?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self::from_bytes(&name, bytes)
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Splits the image into `(load_address, chunk)` pairs of at most
    /// `chunk_size` bytes each, in ascending address order, covering the
    /// whole image with no gaps or overlap — exactly what
    /// [`crate::fx2::upload`] feeds to one vendor-write control transfer per
    /// chunk (SPECIFICATION.md §2.4 step 2).
    ///
    /// # Panics
    /// If `chunk_size` is 0.
    pub fn chunks(&self, chunk_size: usize) -> impl Iterator<Item = (u16, &[u8])> {
        assert!(chunk_size > 0, "chunk_size must be nonzero");
        self.bytes
            .chunks(chunk_size)
            .enumerate()
            .map(move |(i, chunk)| {
                let offset = i * chunk_size;
                // MAX_FIRMWARE_SIZE (64 KiB) keeps every offset in u16 range.
                (offset as u16, chunk)
            })
    }
}

/// Where a resolved firmware blob came from (SPECIFICATION.md §13) — logged
/// so an operator can tell which of the three sources actually supplied a
/// given upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareSource {
    /// Step 1: found under the configured host-mount directory.
    HostMount,
    /// Step 2: found under the configured baked-in directory.
    BakedIn,
    /// Step 3: would have come from a pinned URL, checksum-verified. Not
    /// implemented yet — see [`FirmwareResolver::resolve`].
    AutoDownload,
}

impl fmt::Display for FirmwareSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FirmwareSource::HostMount => "host mount",
            FirmwareSource::BakedIn => "baked-in image",
            FirmwareSource::AutoDownload => "auto-download",
        })
    }
}

/// A firmware blob plus provenance, for logging (SPECIFICATION.md §13: "The
/// resolution chain logs which source supplied each blob").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFirmware {
    pub firmware: Firmware,
    pub source: FirmwareSource,
    pub path: PathBuf,
}

/// Resolves firmware filenames to bytes, trying each source in the order
/// SPECIFICATION.md §13 specifies, first hit wins.
///
/// The override directory and the image's own firmware are kept apart so the
/// log can say which one supplied each blob (SPEC §13) — an operator must be
/// able to tell whether their firmware is actually the one in use. An earlier
/// version pointed both at `/usr/share/sigrok-firmware`, so every blob was
/// reported as "host mount", including the image's own.
pub struct FirmwareResolver {
    host_dir: PathBuf,
    baked_dir: PathBuf,
    auto_download: bool,
}

/// The Debian `sigrok-firmware-fx2lafw` package's install path: the image's
/// baked-in firmware (SPEC §13 step 2).
pub const DEFAULT_FIRMWARE_DIR: &str = "/usr/share/sigrok-firmware";

/// Operator overrides (SPEC §13 step 1): inside the config mount, so dropping
/// a `.fw` into `config/firmware/` on the host is enough — no extra mount.
pub const DEFAULT_HOST_FIRMWARE_DIR: &str = "/etc/awic/firmware";

impl Default for FirmwareResolver {
    fn default() -> Self {
        Self::new(DEFAULT_HOST_FIRMWARE_DIR, DEFAULT_FIRMWARE_DIR, false)
    }
}

impl FirmwareResolver {
    pub fn new(
        host_dir: impl Into<PathBuf>,
        baked_dir: impl Into<PathBuf>,
        auto_download: bool,
    ) -> Self {
        Self {
            host_dir: host_dir.into(),
            baked_dir: baked_dir.into(),
            auto_download,
        }
    }

    /// Builds a resolver from `awic-core`'s config (the `[firmware]`
    /// section carries `auto_download`; the directories are not yet part of
    /// that shared schema, so they take this crate's own defaults, which
    /// match SPECIFICATION.md §13).
    pub fn from_config(cfg: &awic_core::config::FirmwareConfig) -> Self {
        Self::new(
            DEFAULT_HOST_FIRMWARE_DIR,
            DEFAULT_FIRMWARE_DIR,
            cfg.auto_download,
        )
    }

    /// Resolves `filename` (e.g. `"fx2lafw-saleae-logic.fw"`), trying host
    /// mount, then baked-in, then (if enabled) auto-download — first hit
    /// wins, and every step is logged with the source that either supplied
    /// or was tried for the blob.
    pub fn resolve(&self, filename: &str) -> Result<ResolvedFirmware> {
        let mut searched = Vec::new();

        for (dir, source) in [
            (&self.host_dir, FirmwareSource::HostMount),
            (&self.baked_dir, FirmwareSource::BakedIn),
        ] {
            let path = dir.join(filename);
            searched.push(path.display().to_string());
            if path.is_file() {
                let firmware = Firmware::load(&path)?;
                tracing::info!(
                    firmware = filename,
                    source = %source,
                    path = %path.display(),
                    bytes = firmware.len(),
                    "resolved firmware"
                );
                return Ok(ResolvedFirmware {
                    firmware,
                    source,
                    path,
                });
            }
            tracing::debug!(firmware = filename, source = %source, path = %path.display(), "not found");
        }

        if self.auto_download {
            tracing::warn!(
                firmware = filename,
                source = %FirmwareSource::AutoDownload,
                "firmware.auto_download is set, but step 3 is not implemented yet"
            );
            return Err(NativeError::AutoDownloadNotImplemented);
        }

        Err(NativeError::FirmwareNotFound {
            name: filename.to_string(),
            searched,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_firmware() {
        let err = Firmware::from_bytes("x.fw", vec![]).unwrap_err();
        assert!(matches!(err, NativeError::FirmwareEmpty(name) if name == "x.fw"));
    }

    #[test]
    fn rejects_firmware_larger_than_64kib() {
        let err = Firmware::from_bytes("x.fw", vec![0u8; MAX_FIRMWARE_SIZE + 1]).unwrap_err();
        assert!(
            matches!(err, NativeError::FirmwareTooLarge { size, .. } if size == MAX_FIRMWARE_SIZE + 1)
        );
    }

    #[test]
    fn accepts_firmware_at_exactly_the_size_limit() {
        let fw = Firmware::from_bytes("x.fw", vec![0u8; MAX_FIRMWARE_SIZE]).unwrap();
        assert_eq!(fw.len(), MAX_FIRMWARE_SIZE);
    }

    #[test]
    fn chunks_cover_the_whole_image_with_no_gaps_or_overlap() {
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        let fw = Firmware::from_bytes("x.fw", bytes.clone()).unwrap();

        let chunks: Vec<(u16, &[u8])> = fw.chunks(4096).collect();
        assert_eq!(
            chunks.len(),
            3,
            "10000 bytes / 4096 = 3 chunks (2 full + 1 partial)"
        );
        assert_eq!(chunks[0], (0u16, &bytes[0..4096]));
        assert_eq!(chunks[1], (4096u16, &bytes[4096..8192]));
        assert_eq!(chunks[2], (8192u16, &bytes[8192..10000]));

        // Reassembling the chunks must reproduce the original image exactly.
        let mut reassembled = Vec::new();
        for (offset, chunk) in fw.chunks(4096) {
            assert_eq!(offset as usize, reassembled.len());
            reassembled.extend_from_slice(chunk);
        }
        assert_eq!(reassembled, bytes);
    }

    #[test]
    fn chunks_the_real_logic_analyzer_image_size_into_exactly_two_pieces() {
        // The real `fx2lafw-saleae-logic.fw` from sigrok-firmware-fx2lafw is
        // 8120 bytes (verified against the installed package on the dev
        // host) — one full 4096-byte chunk plus a 4024-byte remainder.
        let fw = Firmware::from_bytes("fx2lafw-saleae-logic.fw", vec![0xAAu8; 8120]).unwrap();
        let chunks: Vec<(u16, &[u8])> = fw.chunks(MAX_CHUNK_SIZE).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks[0].1.len(), 4096);
        assert_eq!(chunks[1].0, 4096);
        assert_eq!(chunks[1].1.len(), 8120 - 4096);
    }

    #[test]
    fn resolver_finds_firmware_in_host_dir_before_baked_dir() {
        let host = tempfile::tempdir().unwrap();
        let baked = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("a.fw"), [1, 2, 3]).unwrap();
        std::fs::write(baked.path().join("a.fw"), [9, 9, 9]).unwrap();

        let resolver = FirmwareResolver::new(host.path(), baked.path(), false);
        let resolved = resolver.resolve("a.fw").unwrap();
        assert_eq!(resolved.source, FirmwareSource::HostMount);
        assert_eq!(resolved.firmware.as_bytes(), &[1, 2, 3]);
    }

    #[test]
    fn resolver_falls_back_to_baked_dir_when_host_dir_misses() {
        let host = tempfile::tempdir().unwrap();
        let baked = tempfile::tempdir().unwrap();
        std::fs::write(baked.path().join("a.fw"), [9, 9, 9]).unwrap();

        let resolver = FirmwareResolver::new(host.path(), baked.path(), false);
        let resolved = resolver.resolve("a.fw").unwrap();
        assert_eq!(resolved.source, FirmwareSource::BakedIn);
        assert_eq!(resolved.firmware.as_bytes(), &[9, 9, 9]);
    }

    #[test]
    fn resolver_errors_with_not_implemented_when_only_auto_download_could_supply_it() {
        let host = tempfile::tempdir().unwrap();
        let baked = tempfile::tempdir().unwrap();
        let resolver = FirmwareResolver::new(host.path(), baked.path(), true);
        let err = resolver.resolve("missing.fw").unwrap_err();
        assert!(matches!(err, NativeError::AutoDownloadNotImplemented));
    }

    #[test]
    fn resolver_errors_with_not_found_when_auto_download_is_off() {
        let host = tempfile::tempdir().unwrap();
        let baked = tempfile::tempdir().unwrap();
        let resolver = FirmwareResolver::new(host.path(), baked.path(), false);
        let err = resolver.resolve("missing.fw").unwrap_err();
        assert!(matches!(err, NativeError::FirmwareNotFound { name, .. } if name == "missing.fw"));
    }

    /// Regression check against the *real* `sigrok-firmware-fx2lafw`
    /// package (installed on this dev host at the default
    /// `DEFAULT_FIRMWARE_DIR` for exactly this purpose): confirms the file
    /// this crate's `fx2lafw` driver asks for actually exists, resolves via
    /// the default (host-mount-equals-baked-in-path) resolver, is under the
    /// 64 KiB limit, and — the fact this module's doc comment leads with —
    /// is a raw binary blob rather than Intel HEX or any other framed
    /// format (no `:` record marker at the start, and its length doesn't
    /// look like ASCII-hex-encoded text). Skips gracefully if the package
    /// isn't installed, since CI/other hosts won't have it.
    #[test]
    fn real_logic_analyzer_firmware_package_resolves_and_is_raw_binary() {
        let path = std::path::Path::new(DEFAULT_FIRMWARE_DIR).join("fx2lafw-saleae-logic.fw");
        if !path.exists() {
            eprintln!("skipping: {path:?} not present (sigrok-firmware-fx2lafw not installed)");
            return;
        }

        let resolver = FirmwareResolver::default();
        let resolved = resolver.resolve("fx2lafw-saleae-logic.fw").unwrap();
        assert!(resolved.firmware.len() < MAX_FIRMWARE_SIZE);
        assert!(!resolved.firmware.is_empty());

        let bytes = resolved.firmware.as_bytes();
        assert_ne!(
            bytes[0], b':',
            "an Intel-HEX file starts every record with ':' - this must not"
        );
        // Intel HEX is ASCII text (0x20-0x7E plus CR/LF); a raw 8051 binary
        // is expected to contain plenty of control/high bytes. If this
        // firmware were actually HEX-encoded text, every byte would be
        // printable ASCII or CR/LF.
        let looks_like_ascii_text = bytes
            .iter()
            .all(|&b| (0x20..=0x7E).contains(&b) || b == b'\r' || b == b'\n');
        assert!(
            !looks_like_ascii_text,
            "firmware bytes look like ASCII text, not a raw binary 8051 image"
        );
    }
}

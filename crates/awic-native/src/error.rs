//! `awic-native`'s error type.
//!
//! Driver-specific by design, same rationale as `awic-usb`'s `UsbError`:
//! this crate is where `nusb` errors and FX2/fx2lafw protocol failures are
//! allowed to appear. At the `Instrument`/`Backend` trait boundary these are
//! converted to `awic_core::CoreError::Device(String)` — core never names a
//! driver-specific error type (DEVELOPMENT.md §1 rule 1).

use thiserror::Error;

/// Errors from firmware resolution/upload and the fx2lafw driver.
#[derive(Debug, Error)]
pub enum NativeError {
    #[error("firmware {name:?} not found on any resolution path: {searched:?}")]
    FirmwareNotFound { name: String, searched: Vec<String> },

    #[error("firmware {0:?} is empty")]
    FirmwareEmpty(String),

    #[error(
        "firmware {name:?} is {size} bytes, larger than the 64 KiB an FX2 vendor-write wValue can address"
    )]
    FirmwareTooLarge { name: String, size: usize },

    #[error("failed to read firmware {path:?}: {source}")]
    FirmwareIo {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("firmware auto-download is not yet implemented (SPECIFICATION.md §13 step 3); enable a host mount or bake the firmware into the image instead")]
    AutoDownloadNotImplemented,

    #[error("usb control transfer failed: {0}")]
    Transfer(#[from] nusb::transfer::TransferError),

    #[error("usb error: {0}")]
    Usb(#[from] nusb::Error),

    #[error("failed to open device: {0}")]
    Open(nusb::Error),

    #[error(
        "device at bus {bus} port path {port_path:?} did not re-enumerate within {timeout_ms} ms after firmware upload"
    )]
    ReenumerationTimeout {
        bus: u8,
        port_path: Vec<u8>,
        timeout_ms: u64,
    },

    #[error("device not found: bus {bus} port path {port_path:?}")]
    DeviceNotFound { bus: u8, port_path: Vec<u8> },

    #[error("no bulk IN endpoint found on interface {interface} of {device}")]
    NoBulkInEndpoint { device: String, interface: u8 },

    #[error("sample rate {rate_hz} Hz is not supported by this device")]
    UnsupportedSampleRate { rate_hz: u64 },

    #[error("channel selection {channels:?} invalid for an {channel_count}-channel device")]
    InvalidChannelSelection {
        channels: Vec<u8>,
        channel_count: u8,
    },

    #[error("prepare() must complete before {0}()")]
    NotPrepared(&'static str),

    #[error("configure() must complete before {0}()")]
    NotConfigured(&'static str),

    #[error(
        "acquisition on {device} lost data: {dropped_transfers} transfer(s) failed or were short (see logs); refusing to report a capture that may be missing samples"
    )]
    DataLoss {
        device: String,
        dropped_transfers: usize,
    },
}

pub type Result<T> = std::result::Result<T, NativeError>;

impl From<NativeError> for awic_core::CoreError {
    fn from(err: NativeError) -> Self {
        awic_core::CoreError::Device(err.to_string())
    }
}

//! Bench diagnostic: does this firmware answer the FX2-convention EEPROM read
//! (vendor request 0xA2, device-to-host)? Read-only — never writes.

use std::time::Duration;

use nusb::transfer::{ControlIn, ControlType, Recipient};

#[tokio::main]
async fn main() {
    let info = nusb::list_devices()
        .await
        .unwrap()
        .find(|d| {
            d.vendor_id() == 0x1d50
                && d.product_id() == 0x608e
                && d.serial_number() == Some("Hantek 6022BL")
        })
        .expect("oscilloscope with sigrok firmware at 1d50:608e");
    let dev = info.open().await.unwrap();
    for (label, value) in [("0x0000", 0u16), ("0x0008", 8)] {
        let r = dev
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: 0xA2,
                    value,
                    index: 0,
                    length: 48,
                },
                Duration::from_millis(500),
            )
            .await;
        match r {
            Ok(data) => println!(
                "0xA2 @ {label}: {} bytes: {}",
                data.len(),
                data.iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Err(e) => println!("0xA2 @ {label}: {e}"),
        }
    }
}

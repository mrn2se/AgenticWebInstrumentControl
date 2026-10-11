//! TCP integration test for the SCPI transport (SPECIFICATION.md §9): a
//! real `tokio::net::TcpStream` client against a
//! real `awic_server::scpi::listener` on an ephemeral port, backed by
//! `awic-fake`'s `FakeBackend` (the same synthetic analog/digital pair
//! `tests/recorder.rs` uses for `rec.*` over the WebSocket) — proof this is
//! the same `dispatch::handle_request_with_origin` path, not a parallel
//! implementation of USB/acquisition/recorder logic. `FakeBackend` is used
//! (rather than this crate's own minimal `tests/support::TestBackend`)
//! specifically because its instruments actually honor `cfg.continuous`
//! (SPEC §2.3.2) — `TestBackend` only streams a fixed `cfg.samples` and
//! would never produce data for `REC:STAR`, which forces `continuous: true`.
//!
//! Note on the protocol's own asymmetry, which this test's helpers have to
//! respect: a pure setter (`INST:SEL`, `ACQ:RATE`, ...) that succeeds writes
//! **no** response line at all (SPEC §9's line-oriented, pragmatic design —
//! see `awic_scpi::engine`'s doc comment), so a test harness that always
//! reads one line back per line sent would deadlock on the first setter.
//! [`send`] (write-only) and [`query`] (write, then read exactly one line)
//! are kept separate for that reason.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awic_core::Config;
use awic_fake::FakeBackend;
use awic_proto::{UsbPort, UsbPowerResult, UsbProtectResult, UsbTreeNode, UsbTreeResult};
use awic_server::error::UsbOpError;
use awic_server::scpi::listener;
use awic_server::state::AppState;
use awic_server::usb::UsbController;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// A fixed, single-hub topology — enough to exercise `USB:TREE?`'s
/// rendering without needing `awic-usb` (still a stub in this crate, see
/// `src/usb.rs`'s own doc comment).
struct FixedUsb;

#[async_trait]
impl UsbController for FixedUsb {
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError> {
        Ok(UsbTreeResult {
            roots: vec![UsbTreeNode {
                bus: 3,
                port_path: vec![3],
                vid: 0x1d6b,
                pid: 0x0002,
                product: Some("hub".to_string()),
                speed: Some("480".to_string()),
                is_hub: true,
                ppps: Some(true),
                is_instrument: false,
                ports: vec![UsbPort {
                    number: 4,
                    power: Some(true),
                    power_switchable: true,
                    protected: false,
                    protection: None,
                    derived_protection: None,
                    user_choice: None,
                    label: None,
                    dut: false,
                    switchable: true,
                    non_switchable_reason: None,
                    device: None,
                }],
            }],
        })
    }

    async fn power(&self, _hub: &str, _port: u8, _on: bool) -> Result<UsbPowerResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.power"))
    }

    async fn protect(
        &self,
        _hub: &str,
        _port: u8,
        _protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.protect"))
    }
}

/// Starts the SCPI listener on an OS-assigned port; returns a connected
/// client stream and the fake analog device's bus-path id (discovered from
/// the registry rather than hard-coded, so this test doesn't drift if
/// `awic-fake`'s fixed bus number ever changes).
async fn spawn_server() -> (TcpStream, String) {
    let (addr, analog_device) = spawn_server_at().await;
    (TcpStream::connect(addr).await.unwrap(), analog_device)
}

/// [`spawn_server`], returning the listening address instead of one client,
/// for tests that need more than one connection.
async fn spawn_server_at() -> (std::net::SocketAddr, String) {
    let state = AppState::new(
        Config::default(),
        Arc::new(FakeBackend::new()),
        Arc::new(FixedUsb),
    );
    state.bootstrap().await; // discovers the fake analog + digital devices

    let analog_device = state
        .registry()
        .list()
        .into_iter()
        .find(|d| {
            d.capabilities
                .iter()
                .any(|c| matches!(c, awic_proto::CapabilityInfo::Analog { .. }))
        })
        .expect("FakeBackend always reports an analog device")
        .id;

    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = listener::serve_on(state, tcp_listener).await;
    });

    (addr, analog_device)
}

/// Writes one line with no expectation of a response — for setters, which
/// SPEC §9's line-oriented engine answers with nothing on success (see the
/// module doc).
async fn send(writer: &mut (impl tokio::io::AsyncWrite + Unpin), line: &str) {
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
}

/// Writes one line and reads back exactly one response line, with a
/// generous timeout — a hang here means the connection deadlocked, which is
/// itself the failure worth reporting. Only for commands the test knows
/// will produce output (a query that succeeds).
async fn query(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    line: &str,
) -> String {
    send(writer, line).await;
    let mut response = String::new();
    timeout(Duration::from_secs(5), reader.read_line(&mut response))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a response to {line:?}"))
        .unwrap();
    response.trim_end_matches(['\r', '\n']).to_string()
}

#[tokio::test]
async fn idn_catalog_tree_bad_header_and_recorder_round_trip_over_real_tcp() {
    let (client, analog_device) = spawn_server().await;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    let idn = query(&mut write_half, &mut reader, "*IDN?").await;
    assert!(idn.starts_with("AWIC,awic,0,"), "{idn}");

    let catalog = query(&mut write_half, &mut reader, "INST:CAT?").await;
    assert!(catalog.contains(&analog_device), "{catalog}");

    let tree = query(&mut write_half, &mut reader, "USB:TREE?").await;
    assert!(tree.contains("3-3"), "{tree}");
    assert!(tree.contains("port4=on"), "{tree}");

    // A bad header produces no inline response, but shows up on the queue.
    send(&mut write_half, "NOPE:HEADER?").await;
    let err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert!(err.starts_with("-113,"), "{err}");
    let clean = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert_eq!(clean, r#"0,"No error""#);

    // Select the fake analog device, configure it, then start a continuous
    // recording and search it — REC:STAR/REC:SEAR? end to end through the
    // real recorder module, driven by exactly the same dispatch path a
    // WebSocket client's rec.start/rec.search would use.
    send(&mut write_half, &format!(r#"INST:SEL "{analog_device}""#)).await;
    // Two separate lines, deliberately: joining these on one line without a
    // leading `:` before the second `ACQ` would repeat the just-established
    // context prefix (the standard SCPI compound-message rule this engine
    // implements — `awic-scpi`'s own parser tests cover it directly) and
    // resolve to the nonsensical `ACQUIRE:ACQUIRE:CHANNEL`: a real-world
    // SCPI gotcha, not a bug, and exactly why a script is usually better off
    // sending one command per line unless deliberately chaining siblings.
    send(&mut write_half, "ACQ:RATE 1E6").await;
    send(&mut write_half, "ACQ:CHAN (@1)").await;
    let rec_id = query(&mut write_half, &mut reader, "REC:STAR").await;
    assert!(rec_id.parse::<u32>().is_ok(), "{rec_id}");

    // Give the fake instrument's continuous stream a moment to append data.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let search = query(
        &mut write_half,
        &mut reader,
        r#"REC:SEAR? "CH1 rising 1.2V""#,
    )
    .await;
    // Whatever it finds (a comma list, or nothing), the point is that the
    // search itself completed instead of hanging or erroring — the fixture
    // is a synthetic waveform, not a controlled edge, so asserting an exact
    // match count would test `awic-fake`'s signal, not this transport.
    let _ = search;
    let after_search_err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert_eq!(
        after_search_err, r#"0,"No error""#,
        "the whole INST:SEL -> ACQ:* -> REC:STAR -> REC:SEAR? sequence must be clean"
    );

    send(&mut write_half, "REC:STOP").await;
}

/// SPEC §9's own sequence, with no pause: `ACQ:STAR` then `MEAS:...?` at
/// once. `ACQ:STAR` used to answer as soon as the acquisition *started*, so
/// the `MEAS?` right behind it failed ("capture not found") and, being a
/// query error, answered nothing — a script hung (live test, 2026-09-15).
#[tokio::test]
async fn a_measurement_right_after_acq_star_sees_the_finished_capture() {
    let (client, analog_device) = spawn_server().await;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    send(&mut write_half, &format!(r#"INST:SEL "{analog_device}""#)).await;
    send(&mut write_half, "ACQ:RATE 1E6").await;
    send(&mut write_half, "ACQ:CHAN (@1)").await;
    send(&mut write_half, "ACQ:SAMP 200000").await;
    let capture = query(&mut write_half, &mut reader, "ACQ:STAR").await;
    assert!(capture.starts_with("capture://"), "{capture}");
    let vpp = query(&mut write_half, &mut reader, "MEAS:VPP? CH1").await;
    assert!(vpp.parse::<f64>().is_ok(), "{vpp}");
    let err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert_eq!(err, r#"0,"No error""#);
}

/// A recording outlives the SCPI connection that started it; `REC:SEL`
/// lets another connection pick it up — here, to stop it and free the
/// device, which used to need MCP or the web UI (live test, 2026-09-15).
#[tokio::test]
async fn rec_sel_reaches_a_recording_from_another_connection() {
    let (addr, analog_device) = spawn_server_at().await;
    let rec_id = {
        let (read_half, mut write_half) = TcpStream::connect(addr).await.unwrap().into_split();
        let mut reader = BufReader::new(read_half);
        send(&mut write_half, &format!(r#"INST:SEL "{analog_device}""#)).await;
        send(&mut write_half, "ACQ:RATE 1E6").await;
        send(&mut write_half, "ACQ:CHAN (@1)").await;
        query(&mut write_half, &mut reader, "REC:STAR").await
    };
    assert!(rec_id.parse::<u32>().is_ok(), "{rec_id}");

    let (read_half, mut write_half) = TcpStream::connect(addr).await.unwrap().into_split();
    let mut reader = BufReader::new(read_half);
    let list = query(&mut write_half, &mut reader, "REC:LIST?").await;
    assert!(list.contains(&format!("{rec_id},")), "{list}");
    send(&mut write_half, &format!("REC:SEL {rec_id}")).await;
    assert_eq!(
        query(&mut write_half, &mut reader, "REC:SEL?").await,
        rec_id
    );
    send(&mut write_half, "REC:STOP").await;
    let err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert_eq!(err, r#"0,"No error""#);
    let list = query(&mut write_half, &mut reader, "REC:LIST?").await;
    assert!(list.contains("stopped"), "{list}");
}

/// `INST:SEL` takes a bus path, but a script is more likely to name the
/// instrument; an unambiguous part of its label works, and a name that
/// matches nothing is refused with the catalog (live test B-09).
#[tokio::test]
async fn instruments_can_be_selected_by_name_and_a_miss_lists_the_catalog() {
    let (client, analog_device) = spawn_server().await;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    send(&mut write_half, r#"INST:SEL "fake analog""#).await;
    assert_eq!(
        query(&mut write_half, &mut reader, "INST:SEL?").await,
        format!("\"{analog_device}\"")
    );
    let err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert_eq!(err, r#"0,"No error""#);

    send(&mut write_half, r#"INST:SEL "NO SUCH SCOPE""#).await;
    let err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert!(err.starts_with("-224,"), "{err}");
    assert!(
        err.contains(&analog_device),
        "the refusal lists the catalog: {err}"
    );

    // "fake" matches both instruments: ambiguity is refused, not guessed.
    send(&mut write_half, r#"INST:SEL "fake""#).await;
    let err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert!(err.contains("matches 2 instruments"), "{err}");
}

/// `DEC:LIST?`/`DEC:CAPT?` (SPEC §6/§9, T21) over real TCP, against the
/// digital device's D5 UART-like signal (`awic-fake`'s own doc comment:
/// D5 carries `b"AWIC"` at 9600 baud — see `crates/awic-fake/src/digital.rs`).
#[tokio::test]
async fn decode_list_and_decode_capture_over_real_tcp() {
    let (client, _analog_device) = spawn_server().await;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = BufReader::new(read_half);

    let digital_device = awic_fake::DigitalDeviceSpec::logic_8ch_like()
        .id
        .to_string();

    let list = query(&mut write_half, &mut reader, "DEC:LIST?").await;
    assert_eq!(list, r#""UART","I2C","SPI","CAN","1-Wire""#);

    send(&mut write_half, &format!(r#"INST:SEL "{digital_device}""#)).await;
    send(&mut write_half, "ACQ:RATE 1E6").await;
    // D6 on the wire (1-based) is D5 (0-based) — the UART channel.
    send(&mut write_half, "ACQ:CHAN (@6)").await;
    send(&mut write_half, "ACQ:SAMP 50000").await;
    let capture = query(&mut write_half, &mut reader, "ACQ:STAR").await;
    assert!(capture.starts_with("capture://"), "{capture}");

    // ACQ:STAR blocks until the finite acquisition streams into the capture
    // store (mirrors the WebSocket `acq.start` contract for a small,
    // non-continuous capture); a short settle is still cheap insurance.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Hardware channel 5 (0-based, "D5") is 1-based channel 6 in the
    // `(@n)` convention `DEC:CAPT?` shares with `ACQ:CHAN`/`MEAS`. `DEC:CAPT?`
    // is deliberately kept small (SPEC T21) and takes no decoder options, so
    // this decodes at UART's *default* 115200 baud against D5's actual 9600
    // baud signal — a deliberate mismatch, the SCPI-transport counterpart of
    // the hardware section's "decode degrades sanely" check: the wrong-baud
    // decode must complete cleanly (no server error, no panic, non-empty
    // bounded text), even though the text is not the AWIC message.
    let decoded = query(&mut write_half, &mut reader, r#"DEC:CAPT? "UART",(@6)"#).await;
    assert!(!decoded.is_empty(), "{decoded}");

    let after_err = query(&mut write_half, &mut reader, "SYST:ERR?").await;
    assert_eq!(after_err, r#"0,"No error""#, "{decoded}");
}

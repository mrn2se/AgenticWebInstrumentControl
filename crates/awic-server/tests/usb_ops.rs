//! `usb.cycle` and `usb.label` (live test F-08), against a mock controller
//! that actually models a port: power that can be switched, a device that
//! takes time to come back, and persisted labels. The point is the dispatch
//! logic — off, wait, on, then wait for re-enumeration, and the label a
//! caller sees before switching anything — not `awic-usb` itself.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use awic_core::{Config, PortOverrides};
use awic_proto::{
    Request, ResponseResult, UsbLabelResult, UsbPort, UsbPowerResult, UsbProtectResult,
    UsbTreeNode, UsbTreeResult,
};
use awic_server::backend::PlaceholderBackend;
use awic_server::dispatch::handle_request;
use awic_server::error::UsbOpError;
use awic_server::state::AppState;
use awic_server::usb::UsbController;
use awic_server::ws::WsOut;
use tokio::sync::mpsc;

const HUB: &str = "3-3.4";
const PORT: u8 = 4;
/// How long the mock's "board" takes to enumerate after power returns.
const ENUMERATION_DELAY: Duration = Duration::from_millis(300);

struct MockUsb {
    state: Mutex<MockState>,
}

struct MockState {
    powered: bool,
    powered_at: Option<Instant>,
    overrides: PortOverrides,
    /// Refuse to switch, as a protected port does.
    protected: bool,
    /// How long switching *on* blocks before returning — on the bench the
    /// kernel's port-enable can return only after the device re-enumerated.
    on_blocks_for: Duration,
}

impl MockUsb {
    fn new(protected: bool) -> Arc<Self> {
        Self::with_blocking_on(protected, Duration::ZERO)
    }

    fn with_blocking_on(protected: bool, on_blocks_for: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MockState {
                powered: true,
                powered_at: Some(Instant::now() - ENUMERATION_DELAY),
                overrides: PortOverrides::default(),
                protected,
                on_blocks_for,
            }),
        })
    }
}

#[async_trait]
impl UsbController for MockUsb {
    async fn tree(&self) -> Result<UsbTreeResult, UsbOpError> {
        let s = self.state.lock().unwrap();
        let attached = s.powered
            && s.powered_at
                .is_some_and(|t| t.elapsed() >= ENUMERATION_DELAY);
        let label = s.overrides.label(HUB, PORT);
        Ok(UsbTreeResult {
            roots: vec![UsbTreeNode {
                bus: 3,
                port_path: vec![3, 4],
                vid: 0x0bda,
                pid: 0x5411,
                product: Some("hub".into()),
                speed: Some("480".into()),
                is_hub: true,
                ppps: Some(true),
                is_instrument: false,
                ports: vec![UsbPort {
                    number: PORT,
                    power: Some(s.powered),
                    power_switchable: true,
                    protected: s.protected,
                    protection: s.protected.then(|| "locked by the user".to_string()),
                    derived_protection: None,
                    user_choice: None,
                    label: label.and_then(|l| l.label.clone()),
                    dut: label.is_some_and(|l| l.dut),
                    switchable: !s.protected,
                    non_switchable_reason: None,
                    device: attached.then(|| {
                        Box::new(UsbTreeNode {
                            bus: 3,
                            port_path: vec![3, 4, 4],
                            vid: 0x1209,
                            pid: 0x0002,
                            product: Some("rustyRPFkey Stick".into()),
                            speed: Some("12".into()),
                            is_hub: false,
                            ppps: None,
                            is_instrument: false,
                            ports: vec![],
                        })
                    }),
                }],
            }],
        })
    }

    async fn power(&self, hub: &str, port: u8, on: bool) -> Result<UsbPowerResult, UsbOpError> {
        let block = {
            let mut s = self.state.lock().unwrap();
            if s.protected {
                return Err(UsbOpError {
                    code: "port_protected".into(),
                    msg: "locked by the user".into(),
                });
            }
            s.powered = on;
            s.powered_at = on.then(Instant::now);
            if on {
                s.on_blocks_for
            } else {
                Duration::ZERO
            }
        };
        tokio::time::sleep(block).await;
        Ok(UsbPowerResult {
            hub: hub.to_string(),
            port,
            on,
        })
    }

    async fn protect(
        &self,
        _hub: &str,
        _port: u8,
        _protected: Option<bool>,
    ) -> Result<UsbProtectResult, UsbOpError> {
        Err(UsbOpError::not_implemented("usb.protect"))
    }

    async fn label(
        &self,
        hub: &str,
        port: u8,
        label: Option<String>,
        dut: Option<bool>,
    ) -> Result<UsbLabelResult, UsbOpError> {
        let mut s = self.state.lock().unwrap();
        let entry = s.overrides.set_label(hub, port, label, dut);
        Ok(UsbLabelResult {
            hub: hub.to_string(),
            port,
            label: entry.label,
            dut: entry.dut,
        })
    }
}

fn state(usb: Arc<MockUsb>) -> AppState {
    AppState::new(Config::default(), Arc::new(PlaceholderBackend), usb)
}

fn out() -> mpsc::Sender<WsOut> {
    mpsc::channel(8).0
}

#[tokio::test]
async fn a_labelled_port_shows_its_label_in_the_tree_and_after_a_cycle() {
    let usb = MockUsb::new(false);
    let state = state(usb);

    let response = handle_request(
        &state,
        &out(),
        0,
        Request::UsbLabel {
            v: 1,
            id: 1,
            hub: HUB.into(),
            port: PORT,
            label: Some("DUT: RP2350 test board".into()),
            dut: Some(true),
        },
    )
    .await;
    assert!(response.ok, "{:?}", response.error);

    let tree = handle_request(&state, &out(), 0, Request::UsbTree { v: 1, id: 2 }).await;
    let Some(ResponseResult::UsbTree(tree)) = tree.result else {
        panic!("expected a tree");
    };
    let port = &tree.roots[0].ports[0];
    assert_eq!(port.label.as_deref(), Some("DUT: RP2350 test board"));
    assert!(port.dut);

    // The cycle answers what came back on the port, how long it took, and
    // what the port is — so a caller sees it reset the board under test.
    let started = Instant::now();
    let cycled = handle_request(
        &state,
        &out(),
        0,
        Request::UsbCycle {
            v: 1,
            id: 3,
            hub: HUB.into(),
            port: PORT,
            off_ms: Some(50),
            wait_s: Some(5.0),
        },
    )
    .await;
    assert!(cycled.ok, "{:?}", cycled.error);
    let Some(ResponseResult::UsbCycle(result)) = cycled.result else {
        panic!("expected a cycle result");
    };
    assert_eq!(result.off_ms, 50);
    assert!(result.dut);
    assert_eq!(result.label.as_deref(), Some("DUT: RP2350 test board"));
    let enumerated_after_s = result.enumerated_after_s;
    let device = result.device.expect("the board enumerated again");
    assert_eq!((device.vid, device.pid), (0x1209, 0x0002));
    assert!(
        enumerated_after_s.unwrap() >= ENUMERATION_DELAY.as_secs_f64(),
        "{enumerated_after_s:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(50) + ENUMERATION_DELAY);

    // And the port is powered again, not left off.
    let tree = handle_request(&state, &out(), 0, Request::UsbTree { v: 1, id: 4 }).await;
    let Some(ResponseResult::UsbTree(tree)) = tree.result else {
        panic!("expected a tree");
    };
    assert_eq!(tree.roots[0].ports[0].power, Some(true));

    // Clearing the label forgets it.
    handle_request(
        &state,
        &out(),
        0,
        Request::UsbLabel {
            v: 1,
            id: 5,
            hub: HUB.into(),
            port: PORT,
            label: None,
            dut: Some(false),
        },
    )
    .await;
    let tree = handle_request(&state, &out(), 0, Request::UsbTree { v: 1, id: 6 }).await;
    let Some(ResponseResult::UsbTree(tree)) = tree.result else {
        panic!("expected a tree");
    };
    assert_eq!(tree.roots[0].ports[0].label, None);
    assert!(!tree.roots[0].ports[0].dut);
}

#[tokio::test]
async fn cycling_a_protected_port_is_refused_before_anything_is_switched() {
    let usb = MockUsb::new(true);
    let state = state(usb);
    let response = handle_request(
        &state,
        &out(),
        0,
        Request::UsbCycle {
            v: 1,
            id: 1,
            hub: HUB.into(),
            port: PORT,
            off_ms: None,
            wait_s: None,
        },
    )
    .await;
    assert!(!response.ok);
    let err = response.error.unwrap();
    assert_eq!(err.code, "port_protected");

    let tree = handle_request(&state, &out(), 0, Request::UsbTree { v: 1, id: 2 }).await;
    let Some(ResponseResult::UsbTree(tree)) = tree.result else {
        panic!("expected a tree");
    };
    assert_eq!(
        tree.roots[0].ports[0].power,
        Some(true),
        "a refused cycle must not have switched the port off"
    );
}

/// B-22 (live test round 3): switching on can block until the board has
/// re-enumerated. Timed from the call's return, the cycle then reported the
/// board "back after 0.02 s" when it had taken ~0.7 s. The time must count
/// from the moment power was asked for.
#[tokio::test]
async fn the_back_after_time_counts_from_switching_on_not_from_the_call_returning() {
    let blocks_for = ENUMERATION_DELAY + Duration::from_millis(200);
    let state = state(MockUsb::with_blocking_on(false, blocks_for));
    let cycled = handle_request(
        &state,
        &out(),
        0,
        Request::UsbCycle {
            v: 1,
            id: 1,
            hub: HUB.into(),
            port: PORT,
            off_ms: Some(50),
            wait_s: Some(5.0),
        },
    )
    .await;
    assert!(cycled.ok, "{:?}", cycled.error);
    let Some(ResponseResult::UsbCycle(result)) = cycled.result else {
        panic!("expected a cycle result");
    };
    assert!(result.device.is_some(), "{result:?}");
    let after = result.enumerated_after_s.expect("a time");
    assert!(
        after >= blocks_for.as_secs_f64(),
        "reported back after {after:.3} s, but switching on alone took {:.3} s",
        blocks_for.as_secs_f64()
    );
}

//! Records one capture from a running AWIC server into `testdata/bench/` as a
//! test fixture (`awic_core::fixtures`): `<name>.json` plus `<name>.bin`.
//!
//! ```text
//! cargo run -p awic-server --example record_fixture -- \
//!     --device 3-3.4.1.1 --rate 4000000 --channels 4,6 --samples 240000 \
//!     --trigger 6,falling,0 --name i2c_ssd1306_4msps \
//!     --description "SSD1306 I2C bus: SDA on D4, SCL on D6 ..."
//! ```
//!
//! It drives the WebSocket protocol (SPEC §3) exactly as the web UI does:
//! `acq.configure`, `acq.start`, then collects the stream's binary sample
//! frames until `acq.state` reports the end. The frames carry full
//! resolution unless the server decimates under backpressure, so any
//! decimated encoding or a gap in the sequence numbers is refused rather
//! than written: a fixture must be exactly what the device produced.
//! `--url` defaults to `ws://127.0.0.1:8080/ws`.

use std::path::PathBuf;
use std::time::Duration;

use awic_core::fixtures::FixtureMeta;
use awic_proto::frame::{decode_frame, Encoding};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

struct Args {
    url: String,
    device: String,
    rate: u64,
    channels: Vec<u8>,
    samples: u64,
    range: Option<f64>,
    trigger: Option<Value>,
    name: String,
    description: String,
    out: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let (mut url, mut device, mut rate, mut channels, mut samples) = (None, None, None, None, None);
    let (mut range, mut trigger, mut name, mut description, mut out) =
        (None, None, None, None, None);
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let k = raw[i].as_str();
        let v = raw
            .get(i + 1)
            .cloned()
            .ok_or(format!("{k} needs a value"))?;
        match k {
            "--url" => url = Some(v),
            "--device" => device = Some(v),
            "--rate" => rate = Some(v),
            "--channels" => channels = Some(v),
            "--samples" => samples = Some(v),
            "--range" => range = Some(v),
            "--trigger" => trigger = Some(v),
            "--name" => name = Some(v),
            "--description" => description = Some(v),
            "--out" => out = Some(v),
            other => return Err(format!("unknown argument {other}")),
        }
        i += 2;
    }
    let need = |v: Option<String>, k: &str| v.ok_or(format!("{k} is required"));
    let trigger = match trigger {
        None => None,
        Some(t) => {
            let p: Vec<&str> = t.split(',').collect();
            let [ch, slope, level] = p.as_slice() else {
                return Err("--trigger is channel,rising|falling,level".into());
            };
            Some(json!({
                "kind": "edge",
                "channel": ch.parse::<u8>().map_err(|e| e.to_string())?,
                "slope": slope,
                "level": level.parse::<f64>().map_err(|e| e.to_string())?,
            }))
        }
    };
    Ok(Args {
        url: url.unwrap_or_else(|| "ws://127.0.0.1:8080/ws".into()),
        device: need(device, "--device")?,
        rate: need(rate, "--rate")?
            .parse()
            .map_err(|e| format!("--rate: {e}"))?,
        channels: need(channels, "--channels")?
            .split(',')
            .map(|c| c.parse().map_err(|e| format!("--channels: {e}")))
            .collect::<Result<_, _>>()?,
        samples: need(samples, "--samples")?
            .parse()
            .map_err(|e| format!("--samples: {e}"))?,
        range: range
            .map(|r| r.parse().map_err(|e| format!("--range: {e}")))
            .transpose()?,
        trigger,
        name: need(name, "--name")?,
        description: need(description, "--description")?,
        out: out
            .map(PathBuf::from)
            .unwrap_or_else(awic_core::fixtures::bench_dir),
    })
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Sends one request and waits for its response, ignoring anything else.
async fn request(ws: &mut Ws, id: u32, body: Value) -> Result<Value, String> {
    let mut body = body;
    body["v"] = json!(1);
    body["id"] = json!(id);
    ws.send(Message::Text(body.to_string().into()))
        .await
        .map_err(|e| e.to_string())?;
    while let Some(msg) = ws.next().await {
        if let Message::Text(t) = msg.map_err(|e| e.to_string())? {
            let v: Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
            if v["id"] == json!(id) {
                return if v["ok"] == json!(true) {
                    Ok(v["result"].clone())
                } else {
                    Err(format!("{}: {}", v["error"]["code"], v["error"]["msg"]))
                };
            }
        }
    }
    Err("connection closed".into())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("record_fixture: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let a = parse_args()?;
    let (mut ws, _) = tokio_tungstenite::connect_async(&a.url)
        .await
        .map_err(|e| format!("connect {}: {e}", a.url))?;

    let mut cfg = json!({ "rate_hz": a.rate, "channels": a.channels, "samples": a.samples });
    if let Some(r) = a.range {
        cfg["ranges_v"] = json!(vec![r; a.channels.len()]);
    }
    if let Some(t) = &a.trigger {
        cfg["trigger"] = t.clone();
    }
    let configured = request(
        &mut ws,
        1,
        json!({ "op": "acq.configure", "device": a.device, "cfg": cfg }),
    )
    .await?;
    let devices = request(&mut ws, 2, json!({ "op": "device.list" })).await?;
    let label = devices["devices"]
        .as_array()
        .and_then(|d| d.iter().find(|d| d["id"] == json!(a.device)))
        .and_then(|d| d["label"].as_str())
        .unwrap_or("")
        .to_string();
    let started = request(&mut ws, 3, json!({ "op": "acq.start", "device": a.device })).await?;
    let stream = started["stream"].as_u64().ok_or("acq.start: no stream")? as u16;

    let rate = configured["cfg"]["rate_hz"].as_u64().unwrap_or(a.rate);
    let channels: Vec<u8> = serde_json::from_value(configured["cfg"]["channels"].clone())
        .map_err(|e| format!("effective channels: {e}"))?;
    let mut raw = Vec::new();
    let (mut samples, mut next_seq, mut encoding) = (0u64, 0u32, None);
    let mut ended = false;
    loop {
        // The end event can overtake the last frames; once it has come, drain
        // until every sample is in or the socket goes quiet.
        let wait = if ended {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(30)
        };
        let msg = match tokio::time::timeout(wait, ws.next()).await {
            Ok(Some(m)) => m.map_err(|e| e.to_string())?,
            Ok(None) => break,
            Err(_) if ended => break,
            Err(_) => return Err("no frames for 30 s".into()),
        };
        match msg {
            Message::Binary(bytes) => {
                let (h, payload) = decode_frame(&bytes).map_err(|e| e.to_string())?;
                if h.stream_id != stream {
                    continue;
                }
                if h.sequence != next_seq {
                    return Err(format!(
                        "frame {} after {}: frames were dropped; refusing to write",
                        h.sequence,
                        next_seq.wrapping_sub(1)
                    ));
                }
                next_seq = next_seq.wrapping_add(1);
                if !matches!(
                    h.encoding,
                    Encoding::PackedDigital | Encoding::U8Interleaved | Encoding::I16
                ) {
                    return Err(format!(
                        "frame {} is decimated ({:?}): the client fell behind; refusing to write",
                        h.sequence, h.encoding
                    ));
                }
                encoding = Some(h.encoding);
                raw.extend_from_slice(payload);
                samples += u64::from(h.sample_count);
                if ended && samples >= a.samples {
                    break;
                }
            }
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
                if v["event"] == json!("acq.state") && v["stream"] == json!(stream) {
                    match v["state"].as_str() {
                        Some("stopped") => ended = true,
                        Some("error") => return Err(format!("acquisition failed: {}", v["msg"])),
                        _ => {}
                    }
                    if ended && samples >= a.samples {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if samples != a.samples {
        return Err(format!("got {samples} of {} samples", a.samples));
    }
    let encoding = match encoding.ok_or("no frames arrived")? {
        Encoding::PackedDigital => "packed_digital",
        Encoding::U8Interleaved => "u8_interleaved",
        _ => "i16",
    };
    let meta = FixtureMeta {
        name: a.name.clone(),
        description: a.description,
        recorded: today(),
        device: a.device,
        device_label: label,
        rate_hz: rate,
        channels,
        encoding: encoding.into(),
        calibration: serde_json::from_value(configured["calibration"].clone())
            .map_err(|e| format!("calibration: {e}"))?,
        samples,
        // The server files an edge-triggered capture with the edge at the
        // midpoint (B-32's software trigger keeps samples/2 of history).
        trigger_position: a.trigger.as_ref().map(|_| a.samples / 2),
        trigger: a.trigger,
    };
    // Round-trip through the loader before writing, so a fixture that would
    // not load is never committed.
    let _ = awic_core::fixtures::to_capture(&meta, raw.clone());
    std::fs::create_dir_all(&a.out).map_err(|e| e.to_string())?;
    let json_path = a.out.join(format!("{}.json", a.name));
    let bin_path = a.out.join(format!("{}.bin", a.name));
    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(&bin_path, &raw).map_err(|e| e.to_string())?;
    println!(
        "{}: {samples} samples, {} bytes",
        json_path.display(),
        raw.len()
    );
    Ok(())
}

/// Today's date without a date crate: the recording machine's `date`.
fn today() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

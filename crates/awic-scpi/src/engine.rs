//! The command engine: matches a parsed [`Command`]'s path against the tree
//! in SPECIFICATION.md §9 (plus the recorder commands), calls out to
//! [`ScpiHost`] for anything that touches the rest of the server, and turns
//! the result into the text a client reads back.
//!
//! One [`Engine`] per connection (it owns a [`Context`]); many connections
//! share one `Arc<dyn ScpiHost>`.
//!
//! **Pragmatic choices, called out once here rather than at every call
//! site** (task brief: "pragmatic rather than strictly IEEE-488.2
//! compliant"):
//! - `*RST` resets this session's own state (selected instrument, pending
//!   acquisition settings, last capture) rather than the instrument itself —
//!   `awic_core::Instrument` has no reset operation, and SCPI's own `*RST`
//!   is defined as "return the device to a known state", which for AWIC's
//!   model is closest to "forget what this console had configured".
//! - By default a failed command (of any kind, query or not) produces **no**
//!   output line — only a queued error. This matches real instruments
//!   closely enough for scripts that check `SYST:ERR?`, and keeps a compound
//!   message's output line count meaningful (one line per *successful*
//!   query). `SYST:COMM:ERR:INL ON` opts into answering a failure inline as
//!   `ERR <code>,"<msg>"` — for queries and for the setters that answer on
//!   success (`ACQ:STAR`, `REC:STAR`), i.e. wherever a client reads a line.

use std::sync::Arc;

use crate::context::Context;
use crate::error::ScpiError;
use crate::host::{EdgeTrigger, Measurement, PortLock, ScpiHost, Slope};
use crate::parser::{parse_message, Command};
use crate::value::{format_scpi_number, format_seconds, quote, Param};

/// SCPI's own sentinel for "the operation would have blocked forever"
/// (`REC:WAIT?` on timeout, SPEC §9's own example).
const TIMEOUT_SENTINEL: f64 = 9.91e37;

pub struct Engine {
    host: Arc<dyn ScpiHost>,
    ctx: Context,
}

impl Engine {
    pub fn new(host: Arc<dyn ScpiHost>) -> Self {
        Self {
            host,
            ctx: Context::new(),
        }
    }

    /// Executes one line of input (one SCPI program message, `;`-separated
    /// commands). Returns the response text to write back — already
    /// newline-free, joined with `;` if the line contained more than one
    /// successful query — or `None` if nothing in the line produced output
    /// (pure setters, or everything failed and only queued errors).
    pub async fn execute_line(&mut self, line: &str) -> Option<String> {
        let commands = match parse_message(line) {
            Ok(cmds) => cmds,
            Err(err) => {
                self.ctx.queue_error(err);
                return None;
            }
        };
        let mut outputs = Vec::new();
        for command in &commands {
            match self.execute_one(command).await {
                Ok(Some(text)) => outputs.push(text),
                Ok(None) => {}
                Err(err) => {
                    // A failed query is silent by default (IEEE 488.2), which
                    // leaves a client blocked on its own timeout; inline mode
                    // answers it instead (live test B-10). Failed *setters*
                    // stay silent either way — a client that doesn't read
                    // after a setter would desynchronize. `ACQ:STAR` and
                    // `REC:STAR` are setters that answer on success (a
                    // handle, an id), so a client does read after them: they
                    // answer their failure too (B-23).
                    if self.ctx.inline_errors && (command.query || answers_on_success(command)) {
                        outputs.push(format!("ERR {}", err.to_wire()));
                    }
                    self.ctx.queue_error(err);
                }
            }
        }
        if outputs.is_empty() {
            None
        } else {
            Some(outputs.join(";"))
        }
    }

    /// Removes and returns every error queued so far (see
    /// [`Context::drain_errors`]) — the `scpi` terminal source's hook for
    /// showing errors inline rather than requiring a human to type
    /// `SYST:ERR?`; the TCP transport never calls this, so `SYST:ERR?`
    /// behaves identically for it as SPEC §9 describes.
    pub fn drain_errors(&mut self) -> Vec<ScpiError> {
        self.ctx.drain_errors()
    }

    async fn execute_one(&mut self, cmd: &Command) -> Result<Option<String>, ScpiError> {
        if cmd.is_common {
            self.execute_common(cmd).await
        } else {
            self.execute_path(cmd).await
        }
    }

    async fn execute_common(&mut self, cmd: &Command) -> Result<Option<String>, ScpiError> {
        let name = cmd.segments[0].mnemonic.as_str();
        match (name, cmd.query) {
            ("IDN", true) => Ok(Some(self.host.idn().await)),
            ("RST", false) => {
                self.ctx.reset();
                Ok(None)
            }
            ("CLS", false) => {
                self.ctx.clear_errors();
                Ok(None)
            }
            // Every command this engine executes has already completed by
            // the time it returns (no overlapped/background SCPI
            // operations), so "operation complete" is unconditionally true —
            // and `*WAI` has nothing to wait for.
            ("OPC", true) => Ok(Some("1".to_string())),
            ("OPC", false) => {
                self.ctx.set_operation_complete();
                Ok(None)
            }
            ("WAI", false) => Ok(None),
            // A destructive read, per IEEE 488.2 §11.5.1.2: a driver that
            // polls `*ESR?` between commands to see whether each was
            // accepted must not be shown the same event twice.
            ("ESR", true) => Ok(Some(self.ctx.take_esr().to_string())),
            ("ESE", true) => Ok(Some(self.ctx.ese().to_string())),
            ("ESE", false) => {
                self.ctx.set_ese(register_byte(cmd)?);
                Ok(None)
            }
            ("SRE", true) => Ok(Some(self.ctx.sre().to_string())),
            ("SRE", false) => {
                self.ctx.set_sre(register_byte(cmd)?);
                Ok(None)
            }
            ("STB", true) => Ok(Some(self.ctx.status_byte().to_string())),
            // There is no power-on self-test to run: the instruments are
            // enumerated and their firmware uploaded by the server long
            // before a SCPI client connects, and a device that failed that
            // is reported through `INST:CAT?` and the error queue instead of
            // as a self-test fault. `0` is IEEE 488.2's "no error found",
            // which is the truthful answer for a query that tests nothing.
            ("TST", true) => Ok(Some("0".to_string())),
            // No installed hardware options: which instruments are on the
            // bench is `INST:CAT?`'s question, not `*OPT?`'s.
            ("OPT", true) => Ok(Some("0".to_string())),
            _ => Err(ScpiError::UndefinedHeader(format!(
                "*{name}{}",
                if cmd.query { "?" } else { "" }
            ))),
        }
    }

    async fn execute_path(&mut self, cmd: &Command) -> Result<Option<String>, ScpiError> {
        let path = cmd.path();
        match path.as_str() {
            "INSTRUMENT:CATALOG" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                let names = self.host.instrument_catalog().await;
                Ok(Some(
                    names.iter().map(|n| quote(n)).collect::<Vec<_>>().join(","),
                ))
            }

            "INSTRUMENT:SELECT" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    Ok(Some(quote(
                        self.ctx.selected_instrument.as_deref().unwrap_or(""),
                    )))
                } else {
                    require_params(cmd, 1)?;
                    let selector = cmd.params[0].as_str()?;
                    let canonical = self.host.select_instrument(selector).await?;
                    self.ctx.selected_instrument = Some(canonical);
                    Ok(None)
                }
            }

            "USB:TREE" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                Ok(Some(self.host.usb_tree_text().await?))
            }

            // `USB:HUB "<path>":PORTn:POWER[?]` — the hub path and port
            // number live on the header itself (SPEC §9's own example).
            "USB:HUB:PORT:POWER" => {
                let hub = header_inline_arg(cmd, 1)?;
                let port = header_suffix(cmd, 2)?;
                if cmd.query {
                    require_params(cmd, 0)?;
                    let on = self.host.usb_port_power_get(&hub, port).await?;
                    Ok(Some(bool_str(on)))
                } else {
                    require_params(cmd, 1)?;
                    let on = cmd.params[0].as_bool()?;
                    self.host.usb_port_power_set(&hub, port, on).await?;
                    Ok(None)
                }
            }

            "USB:HUB:PORT:LOCK" => {
                let hub = header_inline_arg(cmd, 1)?;
                let port = header_suffix(cmd, 2)?;
                require_not_query(cmd)?;
                require_params(cmd, 1)?;
                let mode = parse_lock_mode(&cmd.params[0])?;
                self.host.usb_port_lock(&hub, port, mode).await?;
                Ok(None)
            }

            // `USB:PORT:POW "<hub>",<port>,ON|OFF` — the conventional form,
            // hub/port as ordinary data parameters instead.
            "USB:PORT:POWER" => {
                if cmd.query {
                    require_params(cmd, 2)?;
                    let hub = cmd.params[0].as_str()?;
                    let port = param_port(&cmd.params[1])?;
                    let on = self.host.usb_port_power_get(hub, port).await?;
                    Ok(Some(bool_str(on)))
                } else {
                    require_params(cmd, 3)?;
                    let hub = cmd.params[0].as_str()?.to_string();
                    let port = param_port(&cmd.params[1])?;
                    let on = cmd.params[2].as_bool()?;
                    self.host.usb_port_power_set(&hub, port, on).await?;
                    Ok(None)
                }
            }

            "USB:PORT:LOCK" => {
                require_not_query(cmd)?;
                require_params(cmd, 3)?;
                let hub = cmd.params[0].as_str()?.to_string();
                let port = param_port(&cmd.params[1])?;
                let mode = parse_lock_mode(&cmd.params[2])?;
                self.host.usb_port_lock(&hub, port, mode).await?;
                Ok(None)
            }

            "ACQUIRE:RATE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    let rate = self
                        .ctx
                        .pending_acq
                        .rate_hz
                        .ok_or_else(|| ScpiError::CommandError("no rate configured".into()))?;
                    Ok(Some(format_scpi_number(rate)))
                } else {
                    require_params(cmd, 1)?;
                    let rate = cmd.params[0].as_number()?;
                    if rate <= 0.0 {
                        return Err(ScpiError::DataOutOfRange("rate must be positive".into()));
                    }
                    self.ctx.pending_acq.rate_hz = Some(rate);
                    Ok(None)
                }
            }

            "ACQUIRE:CHANNEL" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    let channels = self.ctx.pending_acq.channels.clone().unwrap_or_default();
                    let text = channels
                        .iter()
                        .map(|c| (c + 1).to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    Ok(Some(format!("(@{text})")))
                } else {
                    require_params(cmd, 1)?;
                    let raw = cmd.params[0].as_channel_list()?;
                    let mut zero_based = Vec::with_capacity(raw.len());
                    for &c in raw {
                        zero_based.push(c.checked_sub(1).ok_or_else(|| {
                            ScpiError::DataOutOfRange("channel numbers start at 1".into())
                        })?);
                    }
                    self.ctx.pending_acq.channels = Some(zero_based);
                    Ok(None)
                }
            }

            // Every scope's VISA driver sets the vertical range; without
            // this the analog instruments could be captured from over SCPI
            // but only ever at their default range. The device snaps the
            // request to the nearest range it has, as it does on the wire.
            "ACQUIRE:RANGE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    Ok(Some(format_scpi_number(
                        self.ctx.pending_acq.range_v.unwrap_or(0.0),
                    )))
                } else {
                    require_params(cmd, 1)?;
                    let volts = cmd.params[0].as_number()?;
                    if volts <= 0.0 {
                        return Err(ScpiError::DataOutOfRange(
                            "range must be greater than 0 V".into(),
                        ));
                    }
                    self.ctx.pending_acq.range_v = Some(volts);
                    Ok(None)
                }
            }

            "ACQUIRE:SAMPLES" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    let samples = self.ctx.pending_acq.samples.unwrap_or(0);
                    Ok(Some(format_scpi_number(samples as f64)))
                } else {
                    require_params(cmd, 1)?;
                    let samples = cmd.params[0].as_number()?;
                    if samples < 0.0 {
                        return Err(ScpiError::DataOutOfRange("samples must be >= 0".into()));
                    }
                    self.ctx.pending_acq.samples = Some(samples as u64);
                    Ok(None)
                }
            }

            "ACQUIRE:TRIGGER:EDGE:SOURCE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    let trig = self.ctx.pending_acq.trigger.unwrap_or_default();
                    let ch = trig
                        .source_channel
                        .ok_or_else(|| ScpiError::CommandError("no trigger source set".into()))?;
                    Ok(Some(format!("CH{}", ch + 1)))
                } else {
                    require_params(cmd, 1)?;
                    let ch = cmd.params[0].as_channel()?;
                    self.ctx
                        .pending_acq
                        .trigger
                        .get_or_insert_with(EdgeTrigger::default)
                        .source_channel = Some(ch);
                    Ok(None)
                }
            }

            "ACQUIRE:TRIGGER:EDGE:SLOPE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    let trig = self.ctx.pending_acq.trigger.unwrap_or_default();
                    let slope = trig
                        .slope
                        .ok_or_else(|| ScpiError::CommandError("no trigger slope set".into()))?;
                    Ok(Some(slope_str(slope).to_string()))
                } else {
                    require_params(cmd, 1)?;
                    let slope = parse_slope(&cmd.params[0])?;
                    self.ctx
                        .pending_acq
                        .trigger
                        .get_or_insert_with(EdgeTrigger::default)
                        .slope = Some(slope);
                    Ok(None)
                }
            }

            "ACQUIRE:TRIGGER:EDGE:LEVEL" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    let trig = self.ctx.pending_acq.trigger.unwrap_or_default();
                    let level = trig
                        .level_v
                        .ok_or_else(|| ScpiError::CommandError("no trigger level set".into()))?;
                    Ok(Some(format_scpi_number(level)))
                } else {
                    require_params(cmd, 1)?;
                    let level = cmd.params[0].as_number()?;
                    self.ctx
                        .pending_acq
                        .trigger
                        .get_or_insert_with(EdgeTrigger::default)
                        .level_v = Some(level);
                    Ok(None)
                }
            }

            "ACQUIRE:START" => {
                require_not_query(cmd)?;
                require_params(cmd, 0)?;
                let device = self.selected_device()?.to_string();
                let settings = self.ctx.pending_acq.clone();
                let capture = self.host.acquire(&device, settings).await?;
                self.ctx.last_capture = Some(capture.clone());
                Ok(Some(capture))
            }

            "ACQUIRE:STOP" => {
                require_not_query(cmd)?;
                require_params(cmd, 0)?;
                let device = self.selected_device()?.to_string();
                self.host.acquire_stop(&device).await?;
                Ok(None)
            }

            "RECORD:START" => {
                require_not_query(cmd)?;
                require_params(cmd, 0)?;
                let device = self.selected_device()?.to_string();
                let settings = self.ctx.pending_acq.clone();
                let rec = self.host.rec_start(&device, settings).await?;
                self.ctx.current_recording = Some(rec);
                Ok(Some(rec.to_string()))
            }

            "RECORD:STOP" => {
                require_not_query(cmd)?;
                require_params(cmd, 0)?;
                let rec = self.current_recording()?;
                self.host.rec_stop(rec).await?;
                Ok(None)
            }

            // `REC:SEL <id>` makes an existing recording — started over MCP,
            // the web UI, or an earlier SCPI connection — this connection's
            // current one for `REC:SEAR?`/`WAIT?`/`SNAP?`/`STOP`. Without it
            // a recording whose SCPI connection dropped kept its device busy
            // with no SCPI way to reach it (live test, 2026-09-15).
            "RECORD:SELECT" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    return Ok(Some(self.current_recording()?.to_string()));
                }
                require_params(cmd, 1)?;
                let id = cmd.params[0].as_number()?;
                if id < 0.0 || id.fract() != 0.0 {
                    return Err(ScpiError::IllegalParameterValue(format!(
                        "recording id must be a non-negative integer, got {id}"
                    )));
                }
                let id = id as u32;
                let known = self.host.rec_list().await;
                if !known.iter().any(|r| r.rec == id) {
                    let ids: Vec<String> = known.iter().map(|r| r.rec.to_string()).collect();
                    return Err(ScpiError::IllegalParameterValue(format!(
                        "no recording {id}; REC:LIST? shows [{}]",
                        ids.join(",")
                    )));
                }
                self.ctx.current_recording = Some(id);
                Ok(None)
            }

            "RECORD:LIST" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                let recordings = self.host.rec_list().await;
                if recordings.is_empty() {
                    return Ok(Some(String::new()));
                }
                // `; `-separated records on one physical line, not one line
                // each: this is the TCP transport's line-oriented contract
                // (SPEC §9) — an embedded `\n` would silently split one
                // query's answer across two reads for any client (a script,
                // an AI assistant) that reads a line at a time.
                Ok(Some(
                    recordings
                        .iter()
                        .map(|r| {
                            format!(
                                "{},{},{},{}",
                                r.rec,
                                quote(&r.device),
                                r.state,
                                format_scpi_number(r.rate_hz as f64)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                ))
            }

            "RECORD:SEARCH" => {
                require_query(cmd)?;
                require_params_range(cmd, 1, 2)?;
                let pattern = cmd.params[0].as_str()?;
                let max = match cmd.params.get(1) {
                    Some(p) => Some(p.as_number()? as u32),
                    None => None,
                };
                let rec = self.current_recording()?;
                let matches = self.host.rec_search(rec, pattern, max).await?;
                // Plain decimal, not scientific (SPEC §9's own example:
                // `3.2140,5.8812`) — see `format_seconds`'s doc comment.
                Ok(Some(
                    matches
                        .iter()
                        .map(|t| format_seconds(*t))
                        .collect::<Vec<_>>()
                        .join(","),
                ))
            }

            "RECORD:WAIT" => {
                require_query(cmd)?;
                require_params(cmd, 2)?;
                let pattern = cmd.params[0].as_str()?;
                let timeout_s = cmd.params[1].as_number()?;
                let rec = self.current_recording()?;
                let outcome = self.host.rec_wait(rec, pattern, timeout_s).await?;
                // A real match is plain seconds (SPEC §9: `41.0021`);
                // timeout is the sentinel in scientific notation, exactly as
                // the spec's own example writes it.
                let text = match outcome {
                    Some(t) => format_seconds(t),
                    None => format_scpi_number(TIMEOUT_SENTINEL),
                };
                Ok(Some(text))
            }

            "RECORD:SNAPSHOT" => {
                require_query(cmd)?;
                require_params(cmd, 2)?;
                let t0 = cmd.params[0].as_number()?;
                let t1 = cmd.params[1].as_number()?;
                let rec = self.current_recording()?;
                let capture = self.host.rec_snapshot(rec, t0, t1).await?;
                self.ctx.last_capture = Some(capture.clone());
                Ok(Some(capture))
            }

            "DECODE:LIST" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                let names = self.host.decoder_list().await;
                Ok(Some(
                    names.iter().map(|n| quote(n)).collect::<Vec<_>>().join(","),
                ))
            }

            // `DEC:CAPT? "<decoder>",(@ch,...)[,"<capture>"]` (SPEC §6, T21):
            // channels bind positionally in the decoder's own declared role
            // order (see `ScpiHost::decode_capture_text`'s doc comment) —
            // deliberately not the `role=ch` shape sketched in the task
            // brief, so this needs no new parameter grammar beyond the
            // `(@...)` channel list every `ACQ:CHAN`-like command already
            // parses. `capture` defaults to the last one, exactly as `MEAS`.
            "DECODE:CAPTURE" => {
                require_query(cmd)?;
                require_params_range(cmd, 2, 3)?;
                let decoder = cmd.params[0].as_str()?.to_string();
                let channels = cmd.params[1].as_channel_list()?.to_vec();
                let capture = match cmd.params.get(2) {
                    Some(p) => p.as_str()?.to_string(),
                    None => self.ctx.last_capture.clone().ok_or_else(|| {
                        ScpiError::CommandError(
                            "no capture available; run ACQ:STAR or REC:SNAP? first".into(),
                        )
                    })?,
                };
                let text = self
                    .host
                    .decode_capture_text(&decoder, &channels, &capture)
                    .await?;
                Ok(Some(text))
            }

            "SYSTEM:ERROR" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                let (code, msg) = self.ctx.pop_error();
                Ok(Some(format!("{code},{}", quote(&msg))))
            }

            // `SYST:COMM:ERR:INL ON|OFF|1|0`, and `?` for the current
            // setting (live test B-10).
            "SYSTEM:COMMUNICATE:ERROR:INLINE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    return Ok(Some(u8::from(self.ctx.inline_errors).to_string()));
                }
                require_params(cmd, 1)?;
                self.ctx.inline_errors = cmd.params[0].as_bool()?;
                Ok(None)
            }

            // SCPI-99 §4.2.2: the version of the standard this instrument
            // claims, `YYYY.V`. Fixed, not derived from AWIC's own version —
            // it says which SCPI a client may assume, not which AWIC this is
            // (`*IDN?` answers that).
            "SYSTEM:VERSION" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                Ok(Some("1999.0".to_string()))
            }

            // SCPI-99 §20's required status structure. AWIC defines no
            // operation or questionable conditions — every device failure it
            // knows about is an error queue entry, which the status byte
            // already summarises through EAV — so the condition and event
            // registers read 0 and the enables are stored so that a driver's
            // preset-and-enable sequence round-trips instead of erroring.
            "STATUS:OPERATION"
            | "STATUS:OPERATION:EVENT"
            | "STATUS:OPERATION:CONDITION"
            | "STATUS:QUESTIONABLE"
            | "STATUS:QUESTIONABLE:EVENT"
            | "STATUS:QUESTIONABLE:CONDITION" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                Ok(Some("0".to_string()))
            }

            "STATUS:OPERATION:ENABLE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    Ok(Some(self.ctx.oper_enable().to_string()))
                } else {
                    self.ctx.set_oper_enable(register_word(cmd)?);
                    Ok(None)
                }
            }

            "STATUS:QUESTIONABLE:ENABLE" => {
                if cmd.query {
                    require_params(cmd, 0)?;
                    Ok(Some(self.ctx.ques_enable().to_string()))
                } else {
                    self.ctx.set_ques_enable(register_word(cmd)?);
                    Ok(None)
                }
            }

            "STATUS:PRESET" => {
                require_params(cmd, 0)?;
                self.ctx.status_preset();
                Ok(None)
            }

            "SYSTEM:ERROR:COUNT" => {
                require_query(cmd)?;
                require_params(cmd, 0)?;
                Ok(Some(self.ctx.error_count().to_string()))
            }

            _ => {
                if let Some(kind) = measurement_kind(&path) {
                    self.execute_measure(cmd, kind).await
                } else {
                    Err(ScpiError::UndefinedHeader(path))
                }
            }
        }
    }

    async fn execute_measure(
        &mut self,
        cmd: &Command,
        kind: Measurement,
    ) -> Result<Option<String>, ScpiError> {
        require_query(cmd)?;
        require_params_range(cmd, 1, 2)?;
        let channel = cmd.params[0].as_channel()?;
        let capture = match cmd.params.get(1) {
            Some(p) => p.as_str()?.to_string(),
            None => self.ctx.last_capture.clone().ok_or_else(|| {
                ScpiError::CommandError(
                    "no capture available; run ACQ:STAR or REC:SNAP? first".into(),
                )
            })?,
        };
        let (value, _unit) = self.host.measure(&capture, channel, kind).await?;
        Ok(Some(format_scpi_number(value)))
    }

    fn selected_device(&self) -> Result<&str, ScpiError> {
        self.ctx.selected_instrument.as_deref().ok_or_else(|| {
            ScpiError::CommandError("no instrument selected; use INST:SEL first".into())
        })
    }

    fn current_recording(&self) -> Result<u32, ScpiError> {
        self.ctx.current_recording.ok_or_else(|| {
            ScpiError::CommandError(
                "no active recording; use REC:STAR, or REC:SEL <id> for one in REC:LIST?".into(),
            )
        })
    }
}

fn require_query(cmd: &Command) -> Result<(), ScpiError> {
    if cmd.query {
        Ok(())
    } else {
        Err(ScpiError::CommandError(format!(
            "{} is query-only (append ?)",
            cmd.path()
        )))
    }
}

/// Setters that answer on success — the client reads a line after them,
/// exactly as after a query, so inline mode must answer their failure too.
fn answers_on_success(cmd: &Command) -> bool {
    !cmd.is_common && matches!(cmd.path().as_str(), "ACQUIRE:START" | "RECORD:START")
}

fn require_not_query(cmd: &Command) -> Result<(), ScpiError> {
    if cmd.query {
        Err(ScpiError::CommandError(format!(
            "{} has no query form",
            cmd.path()
        )))
    } else {
        Ok(())
    }
}

fn require_params(cmd: &Command, exact: usize) -> Result<(), ScpiError> {
    require_params_range(cmd, exact, exact)
}

fn require_params_range(cmd: &Command, min: usize, max: usize) -> Result<(), ScpiError> {
    if cmd.params.len() < min {
        Err(ScpiError::MissingParameter)
    } else if cmd.params.len() > max {
        Err(ScpiError::ParameterNotAllowed(format!(
            "{} takes at most {max} parameter(s), got {}",
            cmd.path(),
            cmd.params.len()
        )))
    } else {
        Ok(())
    }
}

fn header_inline_arg(cmd: &Command, index: usize) -> Result<String, ScpiError> {
    cmd.segments
        .get(index)
        .and_then(|s| s.inline_arg.clone())
        .ok_or(ScpiError::MissingParameter)
}

fn header_suffix(cmd: &Command, index: usize) -> Result<u32, ScpiError> {
    cmd.segments
        .get(index)
        .and_then(|s| s.suffix)
        .ok_or(ScpiError::MissingParameter)
}

fn param_port(p: &Param) -> Result<u32, ScpiError> {
    let n = p.as_number()?;
    if n < 1.0 || n.fract() != 0.0 {
        return Err(ScpiError::DataOutOfRange(format!(
            "port must be a positive integer, got {n}"
        )));
    }
    Ok(n as u32)
}

fn bool_str(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

fn slope_str(slope: Slope) -> &'static str {
    match slope {
        Slope::Rising => "POS",
        Slope::Falling => "NEG",
    }
}

fn parse_slope(p: &Param) -> Result<Slope, ScpiError> {
    match p.as_ident()? {
        "POS" => Ok(Slope::Rising),
        "NEG" => Ok(Slope::Falling),
        other => Err(ScpiError::IllegalParameterValue(format!(
            "expected POS|NEG, got {other}"
        ))),
    }
}

/// `ON`/`OFF` arrive already typed as [`Param::Bool`] (the parameter
/// grammar recognizes them directly); `AUTO` is a bare identifier.
fn parse_lock_mode(p: &Param) -> Result<PortLock, ScpiError> {
    match p {
        Param::Bool(true) => Ok(PortLock::Lock),
        Param::Bool(false) => Ok(PortLock::Unlock),
        Param::Ident(s) if s == "AUTO" => Ok(PortLock::Auto),
        other => Err(ScpiError::IllegalParameterValue(format!(
            "expected ON|OFF|AUTO, got {other:?}"
        ))),
    }
}

/// One 8-bit status-register parameter (`*ESE`, `*SRE`). IEEE 488.2 calls
/// for 0-255; anything else is `-222`, not a silently masked value, so a
/// caller that passes a 16-bit mask by mistake hears about it.
fn register_byte(cmd: &Command) -> Result<u8, ScpiError> {
    register_value(cmd, u8::MAX.into()).map(|v| v as u8)
}

/// One 16-bit status-register parameter (`STAT:OPER:ENAB`,
/// `STAT:QUES:ENAB`), whose registers SCPI-99 defines as 16-bit.
fn register_word(cmd: &Command) -> Result<u16, ScpiError> {
    register_value(cmd, u16::MAX.into()).map(|v| v as u16)
}

/// The shared body: a whole number in `0..=max`. Out of range is `-222`
/// rather than a silently masked value, so a caller that passes a 16-bit
/// mask to `*ESE` hears about it instead of setting some other register.
fn register_value(cmd: &Command, max: u32) -> Result<u32, ScpiError> {
    require_params(cmd, 1)?;
    let raw = cmd.params[0].as_number()?;
    if raw < 0.0 || raw > f64::from(max) || raw.fract() != 0.0 {
        return Err(ScpiError::DataOutOfRange(format!(
            "status register takes a whole number 0-{max}, got {raw}"
        )));
    }
    Ok(raw as u32)
}

fn measurement_kind(path: &str) -> Option<Measurement> {
    match path {
        "MEASURE:FREQUENCY" => Some(Measurement::Frequency),
        "MEASURE:PERIOD" => Some(Measurement::Period),
        "MEASURE:VPP" => Some(Measurement::Vpp),
        "MEASURE:DUTY" => Some(Measurement::Duty),
        "MEASURE:VMIN" => Some(Measurement::Vmin),
        "MEASURE:VMAX" => Some(Measurement::Vmax),
        "MEASURE:VAVG" => Some(Measurement::Vavg),
        "MEASURE:VRMS" => Some(Measurement::Vrms),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{AcqSettings, RecSummary};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A host that records every call and answers with fixed/programmable
    /// results — used to test the engine's command dispatch in isolation
    /// from any real server (task brief: "engine tests against a mock
    /// host").
    #[derive(Default)]
    struct MockHost {
        calls: Mutex<Vec<String>>,
        power: Mutex<bool>,
        catalog: Vec<&'static str>,
        wait_result: Mutex<Option<f64>>,
    }

    impl MockHost {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ScpiHost for MockHost {
        async fn idn(&self) -> String {
            "AWIC,awic,0,0.1.0".to_string()
        }
        async fn instrument_catalog(&self) -> Vec<String> {
            self.catalog.iter().map(|s| s.to_string()).collect()
        }
        async fn select_instrument(
            &self,
            selector: &str,
        ) -> Result<String, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("select_instrument({selector})"));
            if self.catalog.contains(&selector) || selector == "1-1" {
                Ok(selector.to_string())
            } else {
                Err(crate::error::HostError::new(
                    1,
                    format!("no such instrument {selector}"),
                ))
            }
        }
        async fn usb_tree_text(&self) -> Result<String, crate::error::HostError> {
            Ok("3-3 hub ppps\n  port1 empty".to_string())
        }
        async fn usb_port_power_set(
            &self,
            hub: &str,
            port: u32,
            on: bool,
        ) -> Result<(), crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("power_set({hub},{port},{on})"));
            *self.power.lock().unwrap() = on;
            Ok(())
        }
        async fn usb_port_power_get(
            &self,
            hub: &str,
            port: u32,
        ) -> Result<bool, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("power_get({hub},{port})"));
            Ok(*self.power.lock().unwrap())
        }
        async fn usb_port_lock(
            &self,
            hub: &str,
            port: u32,
            mode: PortLock,
        ) -> Result<(), crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("lock({hub},{port},{mode:?})"));
            Ok(())
        }
        async fn acquire(
            &self,
            device: &str,
            settings: AcqSettings,
        ) -> Result<String, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("acquire({device},{settings:?})"));
            Ok("capture://11111111-1111-1111-1111-111111111111".to_string())
        }
        async fn acquire_stop(&self, device: &str) -> Result<(), crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("acquire_stop({device})"));
            Ok(())
        }
        async fn measure(
            &self,
            capture: &str,
            channel: u32,
            kind: Measurement,
        ) -> Result<(f64, String), crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("measure({capture},{channel},{kind:?})"));
            Ok((1000.2, "Hz".to_string()))
        }
        async fn rec_start(
            &self,
            device: &str,
            settings: AcqSettings,
        ) -> Result<u32, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rec_start({device},{settings:?})"));
            Ok(7)
        }
        async fn rec_stop(&self, rec: u32) -> Result<(), crate::error::HostError> {
            self.calls.lock().unwrap().push(format!("rec_stop({rec})"));
            Ok(())
        }
        async fn rec_list(&self) -> Vec<RecSummary> {
            vec![RecSummary {
                rec: 7,
                device: "1-1".into(),
                state: "recording".into(),
                rate_hz: 1_000_000,
            }]
        }
        async fn rec_search(
            &self,
            rec: u32,
            pattern: &str,
            max: Option<u32>,
        ) -> Result<Vec<f64>, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rec_search({rec},{pattern},{max:?})"));
            Ok(vec![3.214, 5.8812])
        }
        async fn rec_wait(
            &self,
            rec: u32,
            pattern: &str,
            timeout_s: f64,
        ) -> Result<Option<f64>, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rec_wait({rec},{pattern},{timeout_s})"));
            Ok(*self.wait_result.lock().unwrap())
        }
        async fn rec_snapshot(
            &self,
            rec: u32,
            t0_s: f64,
            t1_s: f64,
        ) -> Result<String, crate::error::HostError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rec_snapshot({rec},{t0_s},{t1_s})"));
            Ok("capture://22222222-2222-2222-2222-222222222222".to_string())
        }
        async fn decoder_list(&self) -> Vec<String> {
            vec!["UART".to_string(), "I2C".to_string(), "SPI".to_string()]
        }
        async fn decode_capture_text(
            &self,
            decoder: &str,
            channels: &[u32],
            capture: &str,
        ) -> Result<String, crate::error::HostError> {
            self.calls.lock().unwrap().push(format!(
                "decode_capture_text({decoder},{channels:?},{capture})"
            ));
            Ok("0x41 0x57 0x49 0x43".to_string())
        }
    }

    fn engine(host: MockHost) -> Engine {
        Engine::new(Arc::new(host))
    }

    #[tokio::test]
    async fn idn_answers_with_the_hosts_identity() {
        let mut eng = engine(MockHost::default());
        assert_eq!(
            eng.execute_line("*IDN?").await,
            Some("AWIC,awic,0,0.1.0".to_string())
        );
    }

    #[tokio::test]
    async fn instrument_catalog_and_select_round_trip() {
        let host = MockHost {
            catalog: vec!["SCOPE2", "LOGIC8"],
            ..Default::default()
        };
        let mut eng = engine(host);
        assert_eq!(
            eng.execute_line("INST:CAT?").await,
            Some(r#""SCOPE2","LOGIC8""#.to_string())
        );
        assert_eq!(eng.execute_line(r#"INST:SEL "LOGIC8""#).await, None);
        assert_eq!(
            eng.execute_line("INST:SEL?").await,
            Some(r#""LOGIC8""#.to_string())
        );
    }

    #[tokio::test]
    async fn selecting_an_unknown_instrument_queues_a_device_error() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line(r#"INST:SEL "NOPE""#).await, None);
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some(r#"1,"no such instrument NOPE""#.to_string())
        );
    }

    #[tokio::test]
    async fn usb_hub_nested_port_power_set_and_query() {
        let mut eng = engine(MockHost::default());
        assert_eq!(
            eng.execute_line(r#"USB:HUB "3-3.4":PORT4:POW OFF"#).await,
            None
        );
        assert_eq!(
            eng.execute_line(r#"USB:HUB "3-3.4":PORT4:POW?"#).await,
            Some("0".to_string())
        );
    }

    #[tokio::test]
    async fn conventional_usb_port_power_form_works_too() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line(r#"USB:PORT:POW "3-3",3,ON"#).await, None);
        assert_eq!(
            eng.execute_line(r#"USB:PORT:POW? "3-3",3"#).await,
            Some("1".to_string())
        );
    }

    #[tokio::test]
    async fn acquire_configure_and_start_flows_through_pending_settings() {
        let host = Arc::new(MockHost::default());
        let mut eng = Engine::new(host.clone());
        eng.execute_line(r#"INST:SEL "1-1""#).await;
        eng.execute_line("ACQ:RATE 24E6").await;
        eng.execute_line("ACQ:CHAN (@1,2)").await;
        eng.execute_line("ACQ:TRIG:EDGE:SOUR CH1;SLOP POS;LEV 1.5")
            .await;
        let capture = eng.execute_line("ACQ:STAR").await.unwrap();
        assert!(capture.starts_with("capture://"));
        assert_eq!(
            eng.execute_line("MEAS:FREQ? CH1").await,
            Some("1.0002E+3".to_string())
        );

        // The accumulated per-command settings must have reached the host
        // in one shot at ACQ:STAR, not been silently dropped.
        let calls = host.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("acquire(1-1,")
                && c.contains("rate_hz: Some(24000000.0)")
                && c.contains("channels: Some([0, 1])")
                && c.contains("source_channel: Some(0)")
                && c.contains("slope: Some(Rising)")
                && c.contains("level_v: Some(1.5)")),
            "acquire() call missing expected settings: {calls:?}"
        );
        assert!(calls.contains(
            &"measure(capture://11111111-1111-1111-1111-111111111111,0,Frequency)".to_string()
        ));
    }

    #[tokio::test]
    async fn bad_header_is_queued_not_returned_inline() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line("FOO:BAR?").await, None);
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some(r#"-113,"FOO""#.to_string())
        );
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some(r#"0,"No error""#.to_string())
        );
    }

    #[tokio::test]
    async fn missing_and_extra_parameters_are_named_errors() {
        let mut eng = engine(MockHost::default());
        eng.execute_line("ACQ:RATE").await; // missing parameter
        assert_eq!(
            eng.execute_line("SYST:ERR?").await.unwrap(),
            r#"-109,"missing parameter""#
        );

        eng.execute_line("ACQ:RATE 1E6,2E6").await; // extra parameter
        let err = eng.execute_line("SYST:ERR?").await.unwrap();
        assert!(err.starts_with("-108,"), "{err}");
    }

    #[tokio::test]
    async fn recorder_commands_round_trip_through_the_host() {
        let mut eng = engine(MockHost::default());
        eng.execute_line(r#"INST:SEL "1-1""#).await;
        assert_eq!(eng.execute_line("REC:STAR").await, Some("7".to_string()));
        assert_eq!(
            eng.execute_line(r#"REC:SEAR? "D0 f=1k 100010x101""#).await,
            Some("3.2140,5.8812".to_string())
        );
        assert_eq!(
            eng.execute_line(r#"REC:WAIT? "D2 rising",10"#).await,
            Some("9.91E+37".to_string()),
            "no match programmed into the mock: must render as the timeout sentinel"
        );
        assert_eq!(eng.execute_line("REC:STOP").await, None);
    }

    #[tokio::test]
    async fn decode_list_and_decode_capture_round_trip_through_the_host() {
        let mut eng = engine(MockHost::default());
        assert_eq!(
            eng.execute_line("DEC:LIST?").await,
            Some(r#""UART","I2C","SPI""#.to_string())
        );

        // No capture yet: DEC:CAPT? without an explicit handle is refused.
        assert_eq!(eng.execute_line(r#"DEC:CAPT? "UART",(@1)"#).await, None);

        eng.execute_line(r#"INST:SEL "1-1""#).await;
        eng.execute_line("ACQ:STAR").await;
        assert_eq!(
            eng.execute_line(r#"DEC:CAPT? "UART",(@1)"#).await,
            Some("0x41 0x57 0x49 0x43".to_string())
        );

        // An explicit capture handle and multiple channels (SPI-shaped) are
        // both accepted and forwarded to the host untouched.
        assert_eq!(
            eng.execute_line(
                r#"DEC:CAPT? "SPI",(@1,2,3),"capture://33333333-3333-3333-3333-333333333333""#
            )
            .await,
            Some("0x41 0x57 0x49 0x43".to_string())
        );
    }

    #[tokio::test]
    async fn a_real_wait_match_is_plain_seconds_not_scientific() {
        let host = MockHost::default();
        *host.wait_result.lock().unwrap() = Some(41.0021);
        let mut eng = engine(host);
        eng.execute_line(r#"INST:SEL "1-1""#).await;
        eng.execute_line("REC:STAR").await;
        assert_eq!(
            eng.execute_line(r#"REC:WAIT? "D2 rising",10"#).await,
            Some("41.0021".to_string())
        );
    }

    #[tokio::test]
    async fn rec_sel_adopts_a_recording_started_elsewhere() {
        let host = MockHost::default();
        *host.wait_result.lock().unwrap() = Some(2.5);
        let mut eng = engine(host);
        // No REC:STAR on this connection: the mock's recording 7 was
        // started by someone else.
        assert_eq!(eng.execute_line("REC:SEL?").await, None);
        let err = eng.execute_line("SYST:ERR?").await.unwrap();
        assert!(err.contains("REC:SEL <id>"), "{err}");
        assert_eq!(eng.execute_line("REC:SEL 3").await, None);
        let err = eng.execute_line("SYST:ERR?").await.unwrap();
        assert!(err.starts_with("-224,") && err.contains("[7]"), "{err}");
        assert_eq!(eng.execute_line("REC:SEL 7").await, None);
        assert_eq!(eng.execute_line("REC:SEL?").await, Some("7".to_string()));
        assert_eq!(
            eng.execute_line(r#"REC:WAIT? "D2 rising",10"#).await,
            Some("2.5000".to_string())
        );
    }

    /// The IEEE 488.2 commands a generic VISA driver issues before it does
    /// anything instrument-specific. Every one of them used to answer
    /// `-113` (and a *query* answering `-113` is silent, so a VISA client
    /// simply timed out) — live test round 4 found that with pyvisa.
    #[tokio::test]
    async fn acq_range_is_settable_and_defaults_to_the_devices_own() {
        let mut eng = engine(MockHost::default());
        // Unset: 0, meaning "whatever the device defaults to".
        assert_eq!(eng.execute_line("ACQ:RANG?").await, Some("0.0E+0".into()));
        assert_eq!(eng.execute_line("ACQ:RANG 2.5").await, None);
        assert_eq!(eng.execute_line("ACQ:RANG?").await, Some("2.5E+0".into()));
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some("0,\"No error\"".into())
        );
        // Long form and case-insensitivity, like every other header.
        assert_eq!(eng.execute_line("acquire:range 5").await, None);
        assert_eq!(eng.execute_line("ACQ:RANG?").await, Some("5.0E+0".into()));
        // A range of zero or less is a range error, not a silent default.
        assert_eq!(eng.execute_line("ACQ:RANG 0").await, None);
        let err = eng.execute_line("SYST:ERR?").await.unwrap();
        assert!(err.starts_with("-222,"), "{err}");
        assert_eq!(eng.execute_line("ACQ:RANG?").await, Some("5.0E+0".into()));
    }

    #[tokio::test]
    async fn the_mandatory_common_commands_all_answer() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line("*TST?").await, Some("0".into()));
        assert_eq!(eng.execute_line("*OPT?").await, Some("0".into()));
        assert_eq!(eng.execute_line("*OPC?").await, Some("1".into()));
        assert_eq!(eng.execute_line("SYST:VERS?").await, Some("1999.0".into()));
        // The setters answer nothing, but must not queue an error either.
        for setter in ["*CLS", "*RST", "*WAI", "*OPC", "*ESE 32", "*SRE 0"] {
            assert_eq!(eng.execute_line(setter).await, None, "{setter}");
        }
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some("0,\"No error\"".into()),
            "a standard startup sequence must leave the queue empty"
        );
    }

    #[tokio::test]
    async fn the_enable_registers_round_trip_and_reject_oversized_masks() {
        let mut eng = engine(MockHost::default());
        eng.execute_line("*ESE 48").await;
        assert_eq!(eng.execute_line("*ESE?").await, Some("48".into()));
        eng.execute_line("*SRE 32").await;
        assert_eq!(eng.execute_line("*SRE?").await, Some("32".into()));
        eng.execute_line("STAT:OPER:ENAB 512").await;
        assert_eq!(
            eng.execute_line("STAT:OPER:ENAB?").await,
            Some("512".into())
        );
        eng.execute_line("STAT:QUES:ENAB 1024").await;
        assert_eq!(
            eng.execute_line("STAT:QUES:ENAB?").await,
            Some("1024".into())
        );
        // SCPI-99: STAT:PRES clears its own enables and leaves *ESE/*SRE be.
        eng.execute_line("STAT:PRES").await;
        assert_eq!(eng.execute_line("STAT:OPER:ENAB?").await, Some("0".into()));
        assert_eq!(eng.execute_line("STAT:QUES:ENAB?").await, Some("0".into()));
        assert_eq!(eng.execute_line("*ESE?").await, Some("48".into()));
        assert_eq!(eng.execute_line("*SRE?").await, Some("32".into()));
        // A 16-bit mask sent to an 8-bit register is refused, not truncated.
        assert_eq!(eng.execute_line("*ESE 256").await, None);
        let err = eng.execute_line("SYST:ERR?").await.unwrap();
        assert!(err.starts_with("-222,"), "{err}");
        assert_eq!(eng.execute_line("*ESE?").await, Some("48".into()));
    }

    /// `*ESR?` is how a driver asks "was the last command accepted?", so the
    /// bit has to follow the error's own class and the read has to clear it.
    #[tokio::test]
    async fn esr_summarises_errors_by_class_and_clears_on_read() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line("*ESR?").await, Some("0".into()));

        // -113, a command error: bit 5.
        eng.execute_line("BOGUS:HEADER").await;
        assert_eq!(eng.execute_line("*ESR?").await, Some("32".into()));
        // Destructive: the same event is not reported twice.
        assert_eq!(eng.execute_line("*ESR?").await, Some("0".into()));

        // -222, an execution error: bit 4.
        eng.execute_line("ACQ:SAMP -1").await;
        assert_eq!(eng.execute_line("*ESR?").await, Some("16".into()));

        // *OPC sets bit 0 by itself.
        eng.execute_line("*OPC").await;
        assert_eq!(eng.execute_line("*ESR?").await, Some("1".into()));

        // *CLS clears the register as well as the queue.
        eng.execute_line("BOGUS:HEADER").await;
        eng.execute_line("*CLS").await;
        assert_eq!(eng.execute_line("*ESR?").await, Some("0".into()));
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some("0,\"No error\"".into())
        );
    }

    #[tokio::test]
    async fn the_status_byte_summarises_through_ese_and_sre() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line("*STB?").await, Some("0".into()));

        // An error queued: EAV (bit 2). ESB stays clear while *ESE is 0.
        eng.execute_line("BOGUS:HEADER").await;
        assert_eq!(eng.execute_line("*STB?").await, Some("4".into()));

        // Enable command errors: ESB (bit 5) joins EAV -> 4 + 32.
        eng.execute_line("*ESE 32").await;
        assert_eq!(eng.execute_line("*STB?").await, Some("36".into()));

        // Enable EAV in *SRE: MSS (bit 6) joins -> 4 + 32 + 64.
        eng.execute_line("*SRE 4").await;
        assert_eq!(eng.execute_line("*STB?").await, Some("100".into()));

        // Reading the ESR drops ESB; draining the queue drops EAV and MSS.
        eng.execute_line("*ESR?").await;
        assert_eq!(eng.execute_line("*STB?").await, Some("68".into()));
        eng.execute_line("SYST:ERR?").await;
        assert_eq!(eng.execute_line("*STB?").await, Some("0".into()));
    }

    /// SCPI-99 requires the registers to exist; AWIC defines no conditions
    /// to put in them, and says so by reading 0 rather than by erroring.
    #[tokio::test]
    async fn the_status_registers_exist_and_read_zero() {
        let mut eng = engine(MockHost::default());
        for query in [
            "STAT:OPER?",
            "STAT:OPER:EVEN?",
            "STAT:OPER:COND?",
            "STAT:QUES?",
            "STAT:QUES:EVEN?",
            "STAT:QUES:COND?",
        ] {
            assert_eq!(eng.execute_line(query).await, Some("0".into()), "{query}");
        }
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some("0,\"No error\"".into())
        );
    }

    #[tokio::test]
    async fn inline_errors_answer_failed_queries_but_never_setters() {
        let mut eng = engine(MockHost::default());
        // Off by default: a failed query is silent (IEEE 488.2).
        assert_eq!(eng.execute_line("MEAS:VPP? CH1").await, None);
        assert_eq!(
            eng.execute_line("SYST:COMM:ERR:INL?").await,
            Some("0".into())
        );

        eng.execute_line("SYST:COMM:ERR:INL ON").await;
        assert_eq!(
            eng.execute_line("SYST:COMM:ERR:INL?").await,
            Some("1".into())
        );
        let reply = eng
            .execute_line("MEAS:VPP? CH1")
            .await
            .expect("inline mode answers a failed query");
        assert!(reply.starts_with("ERR -100,"), "{reply}");
        // The error is still queued, so SYST:ERR? stays authoritative.
        assert!(eng
            .execute_line("SYST:ERR?")
            .await
            .unwrap()
            .starts_with("-100,"));
        // A failed setter stays silent: a client that doesn't read after a
        // setter must not be left with an unread line.
        eng.execute_line("*CLS").await;
        assert_eq!(eng.execute_line("ACQ:RATE").await, None);
        assert!(eng
            .execute_line("SYST:ERR?")
            .await
            .unwrap()
            .starts_with("-109,"));

        eng.execute_line("SYST:COMM:ERR:INL OFF").await;
        assert_eq!(eng.execute_line("MEAS:VPP? CH1").await, None);
    }

    #[tokio::test]
    async fn inline_errors_also_answer_a_failed_acq_star_and_rec_star() {
        let mut eng = engine(MockHost::default());
        eng.execute_line("SYST:COMM:ERR:INL ON").await;
        // No instrument selected: both fail — and a client reads after them
        // (they answer with a handle or an id on success), so they must
        // answer the failure too, not leave the client to time out (B-23).
        let acq = eng
            .execute_line("ACQ:STAR")
            .await
            .expect("an inline answer");
        assert!(acq.starts_with("ERR "), "{acq}");
        let rec = eng
            .execute_line("REC:STAR")
            .await
            .expect("an inline answer");
        assert!(rec.starts_with("ERR "), "{rec}");
        // A plain setter that fails stays silent, as before.
        assert_eq!(eng.execute_line("ACQ:RATE").await, None);
        // And with inline mode off, ACQ:STAR is silent again (IEEE 488.2).
        eng.execute_line("SYST:COMM:ERR:INL OFF").await;
        assert_eq!(eng.execute_line("ACQ:STAR").await, None);
    }

    #[tokio::test]
    async fn drain_errors_empties_the_queue_for_the_terminal_source() {
        let mut eng = engine(MockHost::default());
        eng.execute_line("FOO:BAR?").await;
        eng.execute_line("BAZ:QUUX").await;
        let drained = eng.drain_errors();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].code(), -113);
        // Draining removes them: SYST:ERR? afterwards must be empty.
        assert_eq!(
            eng.execute_line("SYST:ERR?").await,
            Some(r#"0,"No error""#.to_string())
        );
    }

    #[tokio::test]
    async fn error_status_byte_bit_reflects_the_queue() {
        let mut eng = engine(MockHost::default());
        assert_eq!(eng.execute_line("*STB?").await, Some("0".to_string()));
        eng.execute_line("FOO:BAR?").await;
        assert_eq!(eng.execute_line("*STB?").await, Some("4".to_string()));
    }
}

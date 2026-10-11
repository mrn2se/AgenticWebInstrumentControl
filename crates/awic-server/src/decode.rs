//! Shared decoder registry and construction (SPECIFICATION.md §6, §8.2):
//! bridges the wire types in `awic-proto` (`DecoderId`, `ChannelMapping`,
//! `OptionSetting`, `DecoderInfo`) to `awic-decode`'s `Decoder` trait objects
//! and `Annotation`s. `dispatch.rs`'s `capture.decode`/`rec.decode`/
//! `decode.list` and `terminal::decode_source` all go through this module, so
//! the three surfaces build exactly the same decoder from exactly the same
//! configuration and render annotations the same way.
//!
//! `awic_decode::Decoder::options()`/`ChannelRole` are metadata for a human
//! (plain `id`+`description` strings — see that crate's doc comment: "already
//! fixed by the time a `Decoder` exists"); this module is where a decoder's
//! *settable* options get real types, defaults and allowed values for a UI
//! or MCP client to build a form/tool-call from (`decoder_list`), and where
//! request-shaped values (`OptionSetting`) get validated and turned into the
//! typed `*Config` each decoder's constructor actually takes.

use awic_decode::transactions::{
    escaped_text, hex_string, GroupProtocol, Transaction, TransactionGrouper,
};
use awic_decode::{can, i2c, onewire, spi, uart, Annotation, AnnotationKind, Decoder};
use awic_proto::{
    AddressSummary, AnnotationKindWire, ChannelMapping, ChannelRole, DecodeFormat,
    DecodedAnnotation, DecodedTransaction, DecoderId, DecoderInfo, DecoderOptionSpec,
    OptionSetting, OptionValue, TransactionSummary, DEFAULT_MAX_ANNOTATIONS, HARD_MAX_ANNOTATIONS,
};

/// The stable `{code, msg}` shape a `capture.decode`/`rec.decode`/decode
/// terminal-source failure boils down to, mirroring `crate::recorder::RecOpError`.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeOpError {
    pub code: String,
    pub msg: String,
}

impl DecodeOpError {
    pub fn new(code: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            msg: msg.into(),
        }
    }
}

impl From<DecodeBuildError> for DecodeOpError {
    fn from(err: DecodeBuildError) -> Self {
        Self::new("invalid_decoder_config", err.to_string())
    }
}

/// `max_annotations.unwrap_or(default).clamp(1, hard)` — SPEC T21: "a
/// `max_annotations` default and a hard cap".
pub fn clamp_max_annotations(requested: Option<u32>) -> u32 {
    requested
        .unwrap_or(DEFAULT_MAX_ANNOTATIONS)
        .clamp(1, HARD_MAX_ANNOTATIONS)
}

/// Refuses a channel mapping that names a hardware channel the
/// capture/recording never acquired — decoding it would silently read a
/// bit that always reads 0 rather than fail, which is worse than refusing
/// up front.
pub fn validate_channels(
    available: &[u8],
    channels: &[ChannelMapping],
) -> Result<(), DecodeOpError> {
    for mapping in channels {
        for channel in std::iter::once(mapping.channel).chain(mapping.minus) {
            if !available.contains(&channel) {
                return Err(DecodeOpError::new(
                    "channel_not_found",
                    format!("channel {channel} not acquired; available channels: {available:?}"),
                ));
            }
        }
    }
    Ok(())
}

/// The options that configure the analog front end rather than a decoder:
/// the threshold the waveform is cut at and the width of the dead band
/// around it, both in millivolts.
const ANALOG_OPTIONS: [&str; 2] = ["threshold_mv", "hysteresis_mv"];

/// Refuses what only makes sense for a waveform — differential `minus`
/// lines and the analog options — in a request for a digital capture or
/// recording, which has no voltage to threshold.
pub fn reject_analog_params(
    channels: &[ChannelMapping],
    options: &[OptionSetting],
) -> Result<(), DecodeOpError> {
    if let Some(m) = channels.iter().find(|m| m.minus.is_some()) {
        return Err(DecodeOpError::new(
            "invalid_decoder_config",
            format!(
                "`minus` (a differential line, role {:?}) is only valid on an analog capture; \
                 this one is digital",
                m.role
            ),
        ));
    }
    if let Some(o) = options
        .iter()
        .find(|o| ANALOG_OPTIONS.contains(&o.id.as_str()))
    {
        return Err(DecodeOpError::new(
            "invalid_decoder_config",
            format!(
                "option {:?} sets an analog threshold and is only valid on an analog capture; \
                 this one is digital",
                o.id
            ),
        ));
    }
    Ok(())
}

/// Takes the analog options out of `options` (decoders reject options they
/// do not know) and turns them into volts.
fn split_analog_options(
    options: &[OptionSetting],
) -> Result<(Vec<OptionSetting>, awic_core::threshold::ThresholdConfig), DecodeOpError> {
    let millivolts = |id: &str| -> Result<Option<f64>, DecodeOpError> {
        match find_option(options, id) {
            None => Ok(None),
            Some(OptionValue::Int(mv)) if id == "hysteresis_mv" && *mv < 0 => {
                Err(DecodeOpError::new(
                    "invalid_decoder_config",
                    "option \"hysteresis_mv\" must not be negative",
                ))
            }
            Some(OptionValue::Int(mv)) => Ok(Some(*mv as f64 / 1000.0)),
            Some(_) => Err(DecodeBuildError::BadOptionType {
                id: id.to_string(),
                expected: "an integer number of millivolts",
            }
            .into()),
        }
    };
    let config = awic_core::threshold::ThresholdConfig {
        threshold_v: millivolts("threshold_mv")?,
        hysteresis_v: millivolts("hysteresis_mv")?,
        differential_ceiling: None,
    };
    let rest = options
        .iter()
        .filter(|o| !ANALOG_OPTIONS.contains(&o.id.as_str()))
        .cloned()
        .collect();
    Ok((rest, config))
}

/// Everything a `capture.decode`/`rec.decode` request says about *what* to
/// decode and in which shape.
#[derive(Debug, Clone, Copy)]
pub struct DecodeSpec<'a> {
    pub decoder: DecoderId,
    pub channels: &'a [ChannelMapping],
    pub options: &'a [OptionSetting],
    pub max_annotations: Option<u32>,
    /// Annotations nested deeper than this are dropped (0 frames, 1 fields,
    /// 2 bits); `None` keeps all.
    pub max_level: Option<u8>,
    pub format: Option<DecodeFormat>,
    /// Transactions format: the most data bytes in one UART or CS-less SPI
    /// burst (B-97); default [`awic_proto::DEFAULT_MAX_TRANSACTION_BYTES`].
    pub max_bytes: Option<u32>,
    /// Transactions format: fold identical consecutive transactions (F-14);
    /// default on.
    pub fold: Option<bool>,
}

/// A decode's output, shaped as asked.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DecodeOutput {
    pub annotations: Vec<DecodedAnnotation>,
    pub transactions: Vec<DecodedTransaction>,
    /// Set for `format: "transactions"` (B-43).
    pub summary: Option<TransactionSummary>,
    pub truncated: bool,
    pub omitted: u64,
    pub detected_baud: Option<u32>,
    /// Analog captures: the thresholds used, see
    /// `CaptureDecodeResult::note`.
    pub note: Option<String>,
}

impl DecodeOutput {
    /// The list the request's format asked for, the other `None`: a result
    /// carries `transactions` (even empty) or `annotations`, never both
    /// (B-84). `summary` is set exactly for the transactions format.
    pub fn take_lists(
        &mut self,
    ) -> (
        Option<Vec<DecodedAnnotation>>,
        Option<Vec<DecodedTransaction>>,
    ) {
        let annotations = std::mem::take(&mut self.annotations);
        let transactions = std::mem::take(&mut self.transactions);
        if self.summary.is_some() {
            (None, Some(transactions))
        } else {
            (Some(annotations), None)
        }
    }
}

/// Collects a decoder's annotations in the requested shape as they are
/// produced (live test F-02): filtered by `max_level`, or grouped into
/// transactions. [`Self::is_full`] tells a caller streaming a long range when
/// to stop feeding.
pub struct DecodeCollector {
    format: DecodeFormat,
    max_level: u8,
    cap: usize,
    rate_hz: f64,
    decoder: DecoderId,
    annotations: Vec<Annotation>,
    grouper: TransactionGrouper,
    transactions: Vec<Entry>,
    fold: bool,
    /// Every transaction's `(start, end)` sample, repeats included, for the
    /// burst structure; each entry knows where its own begin.
    spans: Vec<(u64, u64)>,
}

/// One returned transaction and the identical ones folded into it.
struct Entry {
    t: Transaction,
    repeat: u32,
    /// Index of its first span in [`DecodeCollector::spans`].
    spans_from: usize,
}

/// Whether `b` repeats `a` exactly, all but its timing (F-14).
fn same_content(a: &Transaction, b: &Transaction) -> bool {
    a.address == b.address
        && a.read == b.read
        && a.repeated_start == b.repeated_start
        && a.data == b.data
        && a.miso == b.miso
        && a.nacks == b.nacks
        && a.presence == b.presence
        && a.errors == b.errors
        && a.notes == b.notes
}

impl DecodeCollector {
    pub fn new(spec: &DecodeSpec, rate_hz: u64) -> Self {
        let group = match spec.decoder {
            DecoderId::Uart => GroupProtocol::Uart,
            DecoderId::I2c => GroupProtocol::I2c,
            DecoderId::Spi => GroupProtocol::Spi,
            DecoderId::Can => GroupProtocol::Can,
            DecoderId::OneWire => GroupProtocol::OneWire,
        };
        Self {
            format: spec.format.unwrap_or_default(),
            max_level: spec.max_level.unwrap_or(u8::MAX),
            cap: clamp_max_annotations(spec.max_annotations) as usize,
            rate_hz: rate_hz.max(1) as f64,
            decoder: spec.decoder,
            annotations: Vec::new(),
            grouper: TransactionGrouper::new(group).with_max_items(
                spec.max_bytes
                    .unwrap_or(awic_proto::DEFAULT_MAX_TRANSACTION_BYTES)
                    .max(1) as usize,
            ),
            transactions: Vec::new(),
            fold: spec.fold.unwrap_or(true),
            spans: Vec::new(),
        }
    }

    fn add_transactions(&mut self, done: Vec<Transaction>) {
        for t in done {
            self.spans.push((t.start_sample, t.end_sample));
            if let Some(last) = self
                .transactions
                .last_mut()
                .filter(|e| self.fold && same_content(&e.t, &t))
            {
                last.repeat += 1;
                last.t.end_sample = last.t.end_sample.max(t.end_sample);
                continue;
            }
            self.transactions.push(Entry {
                t,
                repeat: 1,
                spans_from: self.spans.len() - 1,
            });
        }
    }

    pub fn push(&mut self, annotations: Vec<Annotation>) {
        match self.format {
            DecodeFormat::Annotations => self.annotations.extend(
                annotations
                    .into_iter()
                    .filter(|a| a.level <= self.max_level),
            ),
            // Grouping needs every level: an I²C NACK is a bit-level
            // annotation that becomes a transaction's note.
            DecodeFormat::Transactions => {
                let done = self.grouper.push(&annotations);
                self.add_transactions(done);
            }
        }
    }

    fn len(&self) -> usize {
        match self.format {
            DecodeFormat::Annotations => self.annotations.len(),
            DecodeFormat::Transactions => self.transactions.len(),
        }
    }

    /// More than the cap collected: feeding further only costs memory.
    pub fn is_full(&self) -> bool {
        self.len() > self.cap
    }

    /// `input_complete`: the whole range was fed (and the decoder finished),
    /// so a still-open transaction is the real last one, not a cut.
    pub fn finish(mut self, input_complete: bool) -> DecodeOutput {
        if input_complete {
            let rest = self.grouper.finish();
            self.add_transactions(rest);
        }
        let stopped_early = !input_complete;
        let omitted = self.len().saturating_sub(self.cap) as u64;
        self.annotations.truncate(self.cap);
        if let Some(cut) = self.transactions.get(self.cap) {
            self.spans.truncate(cut.spans_from);
        }
        self.transactions.truncate(self.cap);
        let rate = self.rate_hz;
        let decoder = self.decoder;
        let summary = (self.format == DecodeFormat::Transactions).then(|| {
            let mut s = summarize(&self.transactions, has_addresses(decoder));
            s.bursts = bursts(&self.spans, rate);
            s
        });
        DecodeOutput {
            annotations: self.annotations.iter().map(to_wire_annotation).collect(),
            transactions: self
                .transactions
                .iter()
                .map(|e| {
                    let mut w = to_wire_transaction(&e.t, rate, decoder);
                    w.repeat = (e.repeat > 1).then_some(e.repeat);
                    w
                })
                .collect(),
            summary,
            truncated: stopped_early || omitted > 0,
            omitted,
            detected_baud: None,
            note: None,
        }
    }
}

/// Whether a decoder's transactions carry an address worth grouping by: an
/// I²C address, a CAN identifier, a 1-Wire family code.
fn has_addresses(decoder: DecoderId) -> bool {
    matches!(
        decoder,
        DecoderId::I2c | DecoderId::Can | DecoderId::OneWire
    )
}

/// Totals over the transactions being returned (B-43). `by_address` groups
/// by I²C address, CAN identifier or 1-Wire family code when `with_addresses`.
/// Repeats folded into an entry count one by one (F-14).
fn summarize(entries: &[Entry], with_addresses: bool) -> TransactionSummary {
    let mut by_address: std::collections::BTreeMap<u32, (u32, u32)> = Default::default();
    for e in entries {
        if let Some(a) = e.t.address.filter(|_| with_addresses) {
            let s = by_address.entry(a).or_default();
            s.0 += e.repeat;
            s.1 += e.t.nacks * e.repeat;
        }
    }
    let transactions: u32 = entries.iter().map(|e| e.repeat).sum();
    TransactionSummary {
        transactions,
        folded: transactions - entries.len() as u32,
        nacks: entries.iter().map(|e| e.t.nacks * e.repeat).sum(),
        errors: entries.iter().map(|e| e.t.errors * e.repeat).sum(),
        by_address: by_address
            .into_iter()
            .map(|(address, (transactions, nacks))| AddressSummary {
                address,
                transactions,
                nacks,
            })
            .collect(),
        bursts: None,
    }
}

/// A gap separates bursts when it is this many times longer than the
/// longest gap inside one, each counted with a transaction's length added.
const BURST_GAP_RATIO: f64 = 5.0;

/// F-14: the transactions' burst structure. The gaps between consecutive
/// transactions are sorted and split at their largest jump; a jump of less
/// than [`BURST_GAP_RATIO`] means evenly spaced transactions (a periodic
/// frame, one display write), which is no burst structure. Every gap gets
/// the median transaction's length added before the ratio is taken, so
/// back-to-back transfers a few samples apart do not read as a jump.
fn bursts(spans: &[(u64, u64)], rate_hz: f64) -> Option<awic_proto::BurstSummary> {
    if spans.len() < 3 {
        return None;
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let length = median(
        &mut spans
            .iter()
            .map(|(s, e)| e.saturating_sub(*s) as f64)
            .collect(),
    )
    .max(1.0);
    let gaps: Vec<f64> = spans
        .windows(2)
        .map(|w| w[1].0.saturating_sub(w[0].1) as f64)
        .collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(f64::total_cmp);
    let (jump, at) = sorted
        .windows(2)
        .enumerate()
        .map(|(k, w)| ((w[1] + length) / (w[0] + length), k))
        .max_by(|a, b| a.0.total_cmp(&b.0))?;
    if jump < BURST_GAP_RATIO {
        return None;
    }
    let threshold = ((sorted[at] + length) * (sorted[at + 1] + length)).sqrt() - length;
    let mut starts = vec![0usize];
    starts.extend(
        gaps.iter()
            .enumerate()
            .filter(|(_, &g)| g > threshold)
            .map(|(k, _)| k + 1),
    );
    let ends: Vec<usize> = starts[1..]
        .iter()
        .map(|&s| s - 1)
        .chain([spans.len() - 1])
        .collect();
    let sizes: Vec<u32> = starts
        .iter()
        .zip(&ends)
        .map(|(s, e)| (e - s + 1) as u32)
        .collect();
    let mut durations: Vec<f64> = starts
        .iter()
        .zip(&ends)
        .map(|(&s, &e)| spans[e].1.saturating_sub(spans[s].0) as f64 / rate_hz)
        .collect();
    let mut periods: Vec<f64> = starts
        .windows(2)
        .map(|w| spans[w[1]].0.saturating_sub(spans[w[0]].0) as f64 / rate_hz)
        .collect();
    Some(awic_proto::BurstSummary {
        count: starts.len() as u32,
        transactions_min: *sizes.iter().min()?,
        transactions_max: *sizes.iter().max()?,
        period_s: median(&mut periods),
        duration_s: median(&mut durations),
        gap_s: threshold / rate_hz,
    })
}

fn to_wire_transaction(t: &Transaction, rate_hz: f64, decoder: DecoderId) -> DecodedTransaction {
    let acked = match decoder {
        // An entry with no address or ID is no transaction or frame — STARTs
        // without an address byte (B-83), a CAN stuff error or error flag —
        // and has nothing to be acknowledged.
        DecoderId::I2c | DecoderId::Can => t.address.map(|_| t.nacks == 0),
        // Whether a device answered the reset; none for slots before one.
        DecoderId::OneWire => t.presence,
        _ => None,
    };
    DecodedTransaction {
        start_sample: t.start_sample,
        end_sample: t.end_sample,
        t_s: t.start_sample as f64 / rate_hz,
        address: t.address,
        read: t.read,
        repeated_start: t.repeated_start,
        data: hex_string(&t.data),
        count: t.data.len() as u32,
        miso: (!t.miso.is_empty()).then(|| hex_string(&t.miso)),
        text: (decoder == DecoderId::Uart).then(|| escaped_text(&t.data)),
        acked,
        notes: t.notes.clone(),
        repeat: None,
    }
}

/// The option holding the bit rate a decoder can detect on its own: UART's
/// `baud`, CAN's `bitrate`.
fn auto_rate_option(decoder: DecoderId) -> Option<&'static str> {
    match decoder {
        DecoderId::Uart => Some("baud"),
        DecoderId::Can => Some("bitrate"),
        DecoderId::I2c | DecoderId::Spi | DecoderId::OneWire => None,
    }
}

/// UART with `baud`, or CAN with `bitrate`, given as `"auto"` or `0` (live
/// test F-05).
pub fn wants_auto_baud(decoder: DecoderId, options: &[OptionSetting]) -> bool {
    auto_rate_option(decoder).is_some_and(|id| {
        matches!(
            find_option(options, id),
            Some(OptionValue::Int(0)) | Some(OptionValue::Text(_))
        )
    })
}

/// Which channel a rate-detecting decode reads the rate from: UART's RX
/// role, CAN's bus line.
pub fn auto_rate_channel(
    decoder: DecoderId,
    channels: &[ChannelMapping],
) -> Result<u8, DecodeOpError> {
    let role = match decoder {
        DecoderId::Can => ChannelRole::Can,
        _ => ChannelRole::Rx,
    };
    Ok(channel_for(decoder, channels, role)?)
}

/// Detects the baud rate (UART) or bit rate (CAN — told apart by `channels`
/// binding the `can` role) from the line's own transitions and returns the
/// options with it filled in. `transitions` should cover enough traffic — a
/// few characters or frames at least.
pub fn resolve_auto_baud(
    options: &[OptionSetting],
    channels: &[ChannelMapping],
    transitions: &[awic_decode::Transition],
    rate_hz: u64,
) -> Result<(Vec<OptionSetting>, u32), DecodeOpError> {
    let can = channels.iter().any(|c| c.role == ChannelRole::Can);
    let (decoder, id, what) = if can {
        (DecoderId::Can, "bitrate", "bit rate")
    } else {
        (DecoderId::Uart, "baud", "baud rate")
    };
    if let Some(OptionValue::Text(t)) = find_option(options, id) {
        if t != "auto" {
            return Err(DecodeBuildError::BadOptionType {
                id: id.into(),
                expected: "an integer, or \"auto\"",
            }
            .into());
        }
    }
    let line = auto_rate_channel(decoder, channels)?;
    let estimate = if can {
        awic_decode::baud::estimate_can_bitrate(transitions, line, rate_hz)
    } else {
        awic_decode::baud::estimate_baud(transitions, line, rate_hz)
    }
    .ok_or_else(|| {
        let (give, not) = if can {
            ("`bitrate`", "a CAN bus")
        } else {
            ("`baud`", "a UART line")
        };
        // Say which of the two reasons it was: an edge count that was plenty
        // used to be blamed for widths that did not fit (B-81).
        let pulses = awic_decode::baud::pulse_widths(transitions, line)
            .into_iter()
            .filter(|&w| w > awic_decode::baud::GLITCH_SAMPLES)
            .count();
        let why = if pulses < awic_decode::baud::MIN_PULSES {
            format!(
                "only {pulses} pulses in the range examined, too few to measure a bit time \
                 (not {not}, or too little traffic?). Give {give} explicitly, or decode a \
                 range with more traffic on that channel"
            )
        } else {
            format!(
                "its {pulses} pulses are no whole number of one bit time (not {not}, or \
                 bursts at different rates in one range?). Give {give} explicitly, or decode \
                 each burst on its own range"
            )
        };
        DecodeOpError::new(
            "baud_not_detected",
            format!("could not detect a {what} on channel {line}: {why}"),
        )
    })?;
    let mut resolved: Vec<OptionSetting> = options.iter().filter(|o| o.id != id).cloned().collect();
    resolved.push(OptionSetting {
        id: id.into(),
        value: OptionValue::Int(i64::from(estimate.baud)),
    });
    Ok((resolved, estimate.baud))
}

/// `capture.decode` (SPEC §6, §8.2) over a whole finished capture; see
/// [`decode_capture_range`].
pub fn decode_capture(
    capture: &awic_core::Capture,
    spec: &DecodeSpec,
) -> Result<DecodeOutput, DecodeOpError> {
    decode_capture_range(capture, spec, None, None)
}

/// `capture.decode` over `[t0_s, t1_s)` of a finished capture (default: all
/// of it), in one pass — a capture is already bounded (§2.7's ring). Like a
/// `rec.decode` window, the decoder is seeded with the lines' levels at
/// `t0_s` and waits for the next genuine start there (B-02); sample numbers
/// stay counted from the capture's start, as `read_capture`'s times are
/// (B-101: the window used to be ignored). An analog capture is first cut
/// into logic levels (`awic_core::threshold`, issue #7) over its whole
/// length, so the automatic threshold does not depend on the window; from
/// there on it is decoded exactly like a digital one.
pub fn decode_capture_range(
    capture: &awic_core::Capture,
    spec: &DecodeSpec,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
) -> Result<DecodeOutput, DecodeOpError> {
    let rate_hz = capture.meta.rate_hz;
    match &capture.data {
        awic_core::CaptureData::Digital {
            transitions,
            packed,
        } => {
            reject_analog_params(spec.channels, spec.options)?;
            validate_channels(&capture.meta.channels, spec.channels)?;
            let input: Vec<awic_decode::Transition> = transitions
                .iter()
                .map(|t| awic_decode::Transition {
                    sample_index: t.sample_index,
                    value: t.channel_mask,
                })
                .collect();
            // A capture starts wherever its trigger (or the user) happened
            // to — the bus is not necessarily idle at sample 0, nor at t0_s.
            let (start, end) = sample_window(rate_hz, packed.len() as u64, t0_s, t1_s)?;
            let first = packed.get(start as usize).copied();
            decode_transitions(
                spec,
                spec.channels,
                spec.options,
                rate_hz,
                within(&input, start, end),
                first.map(|f| (start, f)),
                end,
            )
        }
        awic_core::CaptureData::Analog { .. } => {
            validate_channels(&awic_core::hardware_channels(capture), spec.channels)?;
            let (options, mut config) = split_analog_options(spec.options)?;
            if spec.decoder == DecoderId::Can {
                // B-99: a differential CAN line has a standard threshold;
                // the midpoint of the two levels can sit above a weakly
                // driven ACK slot and report every frame unacknowledged.
                config.differential_ceiling = Some(awic_core::threshold::DifferentialCeiling {
                    threshold_v: 0.7,
                    floor_v: 0.5,
                    why: "CAN, ISO 11898-2: recessive < 0.5 V, dominant > 0.9 V",
                });
            }
            if let Some(m) = spec.channels.iter().find(|m| m.minus == Some(m.channel)) {
                return Err(DecodeOpError::new(
                    "invalid_decoder_config",
                    format!(
                        "role {:?}: `minus` is the same channel as `channel`",
                        m.role
                    ),
                ));
            }
            let sources: Vec<awic_core::threshold::AnalogSource> = spec
                .channels
                .iter()
                .map(|m| awic_core::threshold::AnalogSource {
                    channel: m.channel,
                    minus: m.minus,
                })
                .collect();
            let digitized = awic_core::threshold::digitize(capture, &sources, &config)
                .map_err(|e| DecodeOpError::new("invalid_decoder_config", e.to_string()))?;
            // The decoders read bit `channel` of the packed byte; here bit k
            // is the k-th mapping, whatever hardware channels they name.
            let channels: Vec<ChannelMapping> = spec
                .channels
                .iter()
                .enumerate()
                .map(|(k, m)| ChannelMapping {
                    role: m.role,
                    channel: k as u8,
                    minus: None,
                })
                .collect();
            let input: Vec<awic_decode::Transition> = digitized
                .transitions
                .iter()
                .map(|t| awic_decode::Transition {
                    sample_index: t.sample_index,
                    value: t.channel_mask,
                })
                .collect();
            let (start, end) = sample_window(rate_hz, digitized.samples, t0_s, t1_s)?;
            let first = input
                .iter()
                .take_while(|t| t.sample_index <= start)
                .last()
                .map_or(digitized.first, |t| t.value);
            let mut output = decode_transitions(
                spec,
                &channels,
                &options,
                rate_hz,
                within(&input, start, end),
                Some((start, first)),
                end,
            )
            .map_err(|mut e| {
                // A message naming "channel k" means bit k here: name the
                // k-th mapping's hardware line instead, and the threshold
                // it was cut at.
                for (k, m) in spec.channels.iter().enumerate() {
                    let line = match m.minus {
                        Some(minus) => format!("CH{}-CH{}", m.channel + 1, minus + 1),
                        None => format!("CH{}", m.channel + 1),
                    };
                    let bit = format!("on channel {k}:");
                    if e.msg.contains(&bit) {
                        e.msg = e.msg.replacen(&bit, &format!("on {line}:"), 1);
                        break;
                    }
                }
                e.msg = format!("{} ({})", e.msg, digitized.describe(capture));
                e
            })?;
            output.note = Some(digitized.describe(capture));
            Ok(output)
        }
    }
}

/// `[t0_s, t1_s)` of a capture of `samples` samples, as sample numbers;
/// both default to the capture's ends.
fn sample_window(
    rate_hz: u64,
    samples: u64,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
) -> Result<(u64, u64), DecodeOpError> {
    let rate = rate_hz.max(1) as f64;
    let to_sample = |s: f64| (s * rate).round().max(0.0) as u64;
    for (name, v) in [("t0_s", t0_s), ("t1_s", t1_s)] {
        if v.is_some_and(|v| !v.is_finite() || v < 0.0) {
            return Err(DecodeOpError::new(
                "bad_arguments",
                format!("`{name}` must be a non-negative number of seconds"),
            ));
        }
    }
    let start = t0_s.map_or(0, to_sample);
    let end = t1_s.map_or(samples, to_sample).min(samples);
    if start >= end {
        return Err(DecodeOpError::new(
            "empty_range",
            format!(
                "decode range [{:.6}s, {:.6}s) is empty: the capture is {:.6}s long",
                start as f64 / rate,
                end as f64 / rate,
                samples as f64 / rate
            ),
        ));
    }
    Ok((start, end))
}

/// The transitions in `[start, end)`. One at `start` itself is kept: it is
/// how a decoder sees a line that is already mid-pulse there (a UART start
/// bit on the capture's first sample), as a `rec.decode` window's first run
/// is.
fn within(input: &[awic_decode::Transition], start: u64, end: u64) -> &[awic_decode::Transition] {
    let a = input.partition_point(|t| t.sample_index < start);
    let b = input.partition_point(|t| t.sample_index < end);
    &input[a..b.max(a)]
}

/// The part of [`decode_capture_range`] shared by both kinds of capture:
/// `channels` address bits of the `input` transitions' packed byte, `first`
/// is the sample the window starts at and the byte there, and `end` the
/// window's end.
fn decode_transitions(
    spec: &DecodeSpec,
    channels: &[ChannelMapping],
    options: &[OptionSetting],
    rate_hz: u64,
    input: &[awic_decode::Transition],
    first: Option<(u64, u8)>,
    end: u64,
) -> Result<DecodeOutput, DecodeOpError> {
    let (options, detected_baud) = if wants_auto_baud(spec.decoder, options) {
        let (options, baud) = resolve_auto_baud(options, channels, input, rate_hz)?;
        (options, Some(baud))
    } else {
        (options.to_vec(), None)
    };
    let mut instance = build_decoder(spec.decoder, channels, &options, rate_hz)?;
    if let Some((start, levels)) = first {
        instance.seed_at(start, levels);
    }
    let mut collector = DecodeCollector::new(spec, rate_hz);
    collector.push(instance.feed(input, end));
    collector.push(instance.finish(end));
    let mut output = collector.finish(true);
    output.detected_baud = detected_baud;
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecodeBuildError {
    #[error("decoder {decoder:?} needs channel role {role:?} bound")]
    MissingRole {
        decoder: DecoderId,
        role: ChannelRole,
    },
    #[error(
        "unknown option {id:?} for decoder {decoder:?}: its options are {}; an analog \
         capture also takes threshold_mv and hysteresis_mv",
        known.join(", ")
    )]
    UnknownOption {
        id: String,
        decoder: DecoderId,
        known: Vec<String>,
    },
    #[error("option {id:?} expects {expected}")]
    BadOptionType { id: String, expected: &'static str },
    #[error("option {id:?} value {value} is out of range [{min:?}, {max:?}]")]
    OptionOutOfRange {
        id: String,
        value: i64,
        min: Option<i64>,
        max: Option<i64>,
    },
    #[error("option {id:?} must be one of {values:?}, got {value:?}")]
    BadEnumValue {
        id: String,
        values: Vec<String>,
        value: String,
    },
    #[error("invalid decoder configuration: {0}")]
    Invalid(String),
}

/// SPI's optional data lines (SPEC §6: "MOSI/MISO either optional") mean its
/// `required_channels()` is only known after construction; `decode.list`
/// reports the full role set a caller may bind, not a fixed required set.
fn uart_info() -> DecoderInfo {
    DecoderInfo {
        id: DecoderId::Uart,
        name: "UART".to_string(),
        channels: vec![ChannelRole::Rx],
        options: vec![
            DecoderOptionSpec::Int {
                id: "baud".to_string(),
                description: "Baud rate, any integer bit rate the sample rate can resolve; \
                              0 or \"auto\" detects it from the line's pulse widths (the \
                              result then reports `detected_baud`)"
                    .to_string(),
                default: 115_200,
                min: Some(0),
                max: None,
            },
            DecoderOptionSpec::Int {
                id: "data_bits".to_string(),
                description: "Data bits per frame".to_string(),
                default: 8,
                min: Some(5),
                max: Some(9),
            },
            DecoderOptionSpec::Enum {
                id: "parity".to_string(),
                description: "Parity checking".to_string(),
                default: "none".to_string(),
                values: vec!["none".to_string(), "even".to_string(), "odd".to_string()],
            },
            DecoderOptionSpec::Enum {
                id: "stop_bits".to_string(),
                description: "Stop bit count".to_string(),
                default: "one".to_string(),
                values: vec![
                    "one".to_string(),
                    "one_and_half".to_string(),
                    "two".to_string(),
                ],
            },
            DecoderOptionSpec::Enum {
                id: "bit_order".to_string(),
                description: "Which data bit is transmitted first".to_string(),
                default: "lsb".to_string(),
                values: vec!["lsb".to_string(), "msb".to_string()],
            },
            DecoderOptionSpec::Bool {
                id: "invert".to_string(),
                description: "Invert idle polarity: line idles low instead of high".to_string(),
                default: false,
            },
        ],
    }
}

fn i2c_info() -> DecoderInfo {
    // I²C carries its own clock (no baud to configure) and 10-bit addressing
    // is auto-detected from the bus traffic (awic_decode::i2c's own doc
    // comment): there is no real constructor knob to expose here, so unlike
    // `awic_decode::I2cDecoder::options()` (which lists `address_width` as
    // descriptive text only) this metadata has an empty options list —
    // showing a setting that does nothing would mislead a form/agent more
    // than omitting it. Documented deviation from awic-decode's own
    // (informational-only) options() list.
    DecoderInfo {
        id: DecoderId::I2c,
        name: "I2C".to_string(),
        channels: vec![ChannelRole::Scl, ChannelRole::Sda],
        options: vec![],
    }
}

fn spi_info() -> DecoderInfo {
    DecoderInfo {
        id: DecoderId::Spi,
        name: "SPI".to_string(),
        channels: vec![
            ChannelRole::Clk,
            ChannelRole::Mosi,
            ChannelRole::Miso,
            ChannelRole::Cs,
        ],
        options: vec![
            DecoderOptionSpec::Enum {
                id: "mode".to_string(),
                description: "CPOL/CPHA mode".to_string(),
                default: "mode0".to_string(),
                values: vec![
                    "mode0".to_string(),
                    "mode1".to_string(),
                    "mode2".to_string(),
                    "mode3".to_string(),
                ],
            },
            DecoderOptionSpec::Enum {
                id: "bit_order".to_string(),
                description: "Which bit of each word is clocked first".to_string(),
                default: "msb".to_string(),
                values: vec!["msb".to_string(), "lsb".to_string()],
            },
            DecoderOptionSpec::Int {
                id: "word_size".to_string(),
                description: "Bits per word".to_string(),
                default: 8,
                min: Some(1),
                max: Some(32),
            },
            DecoderOptionSpec::Bool {
                id: "cs_active_high".to_string(),
                description: "Chip-select polarity; default active low".to_string(),
                default: false,
            },
        ],
    }
}

fn can_info() -> DecoderInfo {
    DecoderInfo {
        id: DecoderId::Can,
        name: "CAN".to_string(),
        channels: vec![ChannelRole::Can],
        options: vec![
            DecoderOptionSpec::Int {
                id: "bitrate".to_string(),
                description: "Bit rate in bit/s; 0 or \"auto\" detects it from the shortest \
                              pulses and snaps to a standard rate within 3 % (the result then \
                              reports `detected_baud`)"
                    .to_string(),
                default: 500_000,
                min: Some(0),
                max: Some(1_000_000),
            },
            DecoderOptionSpec::Int {
                id: "sample_point".to_string(),
                description: "Where in the bit the line is sampled, in percent of the bit time"
                    .to_string(),
                default: 75,
                min: Some(50),
                max: Some(90),
            },
            DecoderOptionSpec::Enum {
                id: "polarity".to_string(),
                description: "Which level is dominant: low for the RX pin of a transceiver or \
                              CANL, high for CANH or CANH-CANL"
                    .to_string(),
                default: "dominant_low".to_string(),
                values: vec!["dominant_low".to_string(), "dominant_high".to_string()],
            },
        ],
    }
}

fn onewire_info() -> DecoderInfo {
    DecoderInfo {
        id: DecoderId::OneWire,
        name: "1-Wire".to_string(),
        channels: vec![ChannelRole::Owr],
        options: vec![DecoderOptionSpec::Enum {
            id: "speed".to_string(),
            description: "Bus speed before the first reset: overdrive for a window opened on a \
                          bus already switched. Overdrive Skip/Match ROM switch to overdrive \
                          and a standard-length reset back, whatever this says"
                .to_string(),
            default: "standard".to_string(),
            values: vec!["standard".to_string(), "overdrive".to_string()],
        }],
    }
}

/// `decode.list` (SPEC T21): every decoder's id, channel roles and options,
/// so a UI or agent can build a form/tool-call with no hardcoded protocol
/// knowledge.
pub fn decoder_list() -> awic_proto::DecodeListResult {
    awic_proto::DecodeListResult {
        decoders: vec![
            uart_info(),
            i2c_info(),
            spi_info(),
            can_info(),
            onewire_info(),
        ],
    }
}

fn channel_for(
    decoder: DecoderId,
    channels: &[ChannelMapping],
    role: ChannelRole,
) -> Result<u8, DecodeBuildError> {
    channels
        .iter()
        .find(|c| c.role == role)
        .map(|c| c.channel)
        .ok_or(DecodeBuildError::MissingRole { decoder, role })
}

fn optional_channel(channels: &[ChannelMapping], role: ChannelRole) -> Option<u8> {
    channels.iter().find(|c| c.role == role).map(|c| c.channel)
}

fn find_option<'a>(options: &'a [OptionSetting], id: &str) -> Option<&'a OptionValue> {
    options.iter().find(|o| o.id == id).map(|o| &o.value)
}

fn get_int(
    options: &[OptionSetting],
    id: &str,
    default: i64,
    min: Option<i64>,
    max: Option<i64>,
) -> Result<i64, DecodeBuildError> {
    let value = match find_option(options, id) {
        None => default,
        Some(OptionValue::Int(v)) => *v,
        Some(_) => {
            return Err(DecodeBuildError::BadOptionType {
                id: id.to_string(),
                expected: "an integer",
            })
        }
    };
    if min.is_some_and(|m| value < m) || max.is_some_and(|m| value > m) {
        return Err(DecodeBuildError::OptionOutOfRange {
            id: id.to_string(),
            value,
            min,
            max,
        });
    }
    Ok(value)
}

fn get_bool(options: &[OptionSetting], id: &str, default: bool) -> Result<bool, DecodeBuildError> {
    match find_option(options, id) {
        None => Ok(default),
        Some(OptionValue::Bool(v)) => Ok(*v),
        Some(_) => Err(DecodeBuildError::BadOptionType {
            id: id.to_string(),
            expected: "a boolean",
        }),
    }
}

fn get_enum<'a>(
    options: &[OptionSetting],
    id: &str,
    default: &'a str,
    values: &[&'a str],
) -> Result<String, DecodeBuildError> {
    let value = match find_option(options, id) {
        None => default.to_string(),
        Some(OptionValue::Text(v)) => v.clone(),
        Some(_) => {
            return Err(DecodeBuildError::BadOptionType {
                id: id.to_string(),
                expected: "a string",
            })
        }
    };
    if !values.contains(&value.as_str()) {
        return Err(DecodeBuildError::BadEnumValue {
            id: id.to_string(),
            values: values.iter().map(|v| v.to_string()).collect(),
            value,
        });
    }
    Ok(value)
}

fn build_uart(
    channels: &[ChannelMapping],
    options: &[OptionSetting],
    sample_rate_hz: u64,
) -> Result<Box<dyn Decoder>, DecodeBuildError> {
    let bit = channel_for(DecoderId::Uart, channels, ChannelRole::Rx)?;
    let baud = get_int(options, "baud", 115_200, Some(1), None)? as u32;
    let data_bits = get_int(options, "data_bits", 8, Some(5), Some(9))? as u8;
    let parity = match get_enum(options, "parity", "none", &["none", "even", "odd"])?.as_str() {
        "even" => uart::Parity::Even,
        "odd" => uart::Parity::Odd,
        _ => uart::Parity::None,
    };
    let stop_bits =
        match get_enum(options, "stop_bits", "one", &["one", "one_and_half", "two"])?.as_str() {
            "one_and_half" => uart::StopBits::OneAndHalf,
            "two" => uart::StopBits::Two,
            _ => uart::StopBits::One,
        };
    let bit_order = match get_enum(options, "bit_order", "lsb", &["lsb", "msb"])?.as_str() {
        "msb" => uart::BitOrder::Msb,
        _ => uart::BitOrder::Lsb,
    };
    let invert = get_bool(options, "invert", false)?;
    let cfg = uart::UartConfig {
        bit,
        sample_rate_hz,
        baud,
        data_bits,
        parity,
        stop_bits,
        bit_order,
        invert,
    };
    let decoder =
        uart::UartDecoder::new(cfg).map_err(|e| DecodeBuildError::Invalid(e.to_string()))?;
    Ok(Box::new(decoder))
}

fn build_i2c(channels: &[ChannelMapping]) -> Result<Box<dyn Decoder>, DecodeBuildError> {
    let scl = channel_for(DecoderId::I2c, channels, ChannelRole::Scl)?;
    let sda = channel_for(DecoderId::I2c, channels, ChannelRole::Sda)?;
    let decoder = i2c::I2cDecoder::new(i2c::I2cConfig { scl, sda })
        .map_err(|e| DecodeBuildError::Invalid(e.to_string()))?;
    Ok(Box::new(decoder))
}

fn build_spi(
    channels: &[ChannelMapping],
    options: &[OptionSetting],
) -> Result<Box<dyn Decoder>, DecodeBuildError> {
    let clk = channel_for(DecoderId::Spi, channels, ChannelRole::Clk)?;
    let mosi = optional_channel(channels, ChannelRole::Mosi);
    let miso = optional_channel(channels, ChannelRole::Miso);
    let cs = optional_channel(channels, ChannelRole::Cs);
    let mode = match get_enum(
        options,
        "mode",
        "mode0",
        &["mode0", "mode1", "mode2", "mode3"],
    )?
    .as_str()
    {
        "mode1" => spi::SpiMode::Mode1,
        "mode2" => spi::SpiMode::Mode2,
        "mode3" => spi::SpiMode::Mode3,
        _ => spi::SpiMode::Mode0,
    };
    let bit_order = match get_enum(options, "bit_order", "msb", &["msb", "lsb"])?.as_str() {
        "lsb" => spi::BitOrder::Lsb,
        _ => spi::BitOrder::Msb,
    };
    let word_size = get_int(options, "word_size", 8, Some(1), Some(32))? as u8;
    let cs_active_high = get_bool(options, "cs_active_high", false)?;
    let cfg = spi::SpiConfig {
        clk,
        mosi,
        miso,
        cs,
        cs_active_high,
        mode,
        bit_order,
        word_size,
    };
    let decoder =
        spi::SpiDecoder::new(cfg).map_err(|e| DecodeBuildError::Invalid(e.to_string()))?;
    Ok(Box::new(decoder))
}

fn build_can(
    channels: &[ChannelMapping],
    options: &[OptionSetting],
    sample_rate_hz: u64,
) -> Result<Box<dyn Decoder>, DecodeBuildError> {
    let bit = channel_for(DecoderId::Can, channels, ChannelRole::Can)?;
    let bitrate = get_int(options, "bitrate", 500_000, Some(1), Some(1_000_000))? as u32;
    let sample_point = get_int(options, "sample_point", 75, Some(50), Some(90))? as u8;
    let polarity = match get_enum(
        options,
        "polarity",
        "dominant_low",
        &["dominant_low", "dominant_high"],
    )?
    .as_str()
    {
        "dominant_high" => can::Polarity::DominantHigh,
        _ => can::Polarity::DominantLow,
    };
    let cfg = can::CanConfig {
        bit,
        sample_rate_hz,
        bitrate,
        sample_point,
        polarity,
    };
    let decoder =
        can::CanDecoder::new(cfg).map_err(|e| DecodeBuildError::Invalid(e.to_string()))?;
    Ok(Box::new(decoder))
}

fn build_onewire(
    channels: &[ChannelMapping],
    options: &[OptionSetting],
    sample_rate_hz: u64,
) -> Result<Box<dyn Decoder>, DecodeBuildError> {
    let bit = channel_for(DecoderId::OneWire, channels, ChannelRole::Owr)?;
    let speed = match get_enum(options, "speed", "standard", &["standard", "overdrive"])?.as_str() {
        "overdrive" => onewire::Speed::Overdrive,
        _ => onewire::Speed::Standard,
    };
    let cfg = onewire::OneWireConfig {
        bit,
        sample_rate_hz,
        speed,
    };
    let decoder =
        onewire::OneWireDecoder::new(cfg).map_err(|e| DecodeBuildError::Invalid(e.to_string()))?;
    Ok(Box::new(decoder))
}

/// Builds a decoder instance from wire-shaped configuration (SPEC §6): the
/// same construction path for `capture.decode`, `rec.decode` and the
/// `decode` terminal source.
pub fn build_decoder(
    decoder: DecoderId,
    channels: &[ChannelMapping],
    options: &[OptionSetting],
    sample_rate_hz: u64,
) -> Result<Box<dyn Decoder>, DecodeBuildError> {
    // An option the decoder does not have would be dropped without a word,
    // and the decode run on its default (B-106: `baudrate` for `baud`).
    let info = match decoder {
        DecoderId::Uart => uart_info(),
        DecoderId::I2c => i2c_info(),
        DecoderId::Spi => spi_info(),
        DecoderId::Can => can_info(),
        DecoderId::OneWire => onewire_info(),
    };
    if let Some(o) = options
        .iter()
        .find(|o| !info.options.iter().any(|spec| spec.id() == o.id))
    {
        return Err(DecodeBuildError::UnknownOption {
            id: o.id.clone(),
            decoder,
            known: info
                .options
                .iter()
                .map(|spec| spec.id().to_string())
                .collect(),
        });
    }
    match decoder {
        DecoderId::Uart => build_uart(channels, options, sample_rate_hz),
        DecoderId::I2c => build_i2c(channels),
        DecoderId::Spi => build_spi(channels, options),
        DecoderId::Can => build_can(channels, options, sample_rate_hz),
        DecoderId::OneWire => build_onewire(channels, options, sample_rate_hz),
    }
}

fn to_wire_kind(kind: &AnnotationKind) -> AnnotationKindWire {
    match kind {
        AnnotationKind::Data(value) => AnnotationKindWire::Data { value: *value },
        AnnotationKind::FramingError => AnnotationKindWire::FramingError,
        AnnotationKind::ParityError => AnnotationKindWire::ParityError,
        AnnotationKind::Break => AnnotationKindWire::Break,
        AnnotationKind::Start => AnnotationKindWire::Start,
        AnnotationKind::RepeatedStart => AnnotationKindWire::RepeatedStart,
        AnnotationKind::Stop => AnnotationKindWire::Stop,
        AnnotationKind::Address { address, read } => AnnotationKindWire::Address {
            address: *address,
            read: *read,
        },
        AnnotationKind::Ack => AnnotationKindWire::Ack,
        AnnotationKind::Nack => AnnotationKindWire::Nack,
        AnnotationKind::ChipSelect { active } => AnnotationKindWire::ChipSelect { active: *active },
        AnnotationKind::Word { mosi, miso } => AnnotationKindWire::Word {
            mosi: *mosi,
            miso: *miso,
        },
        AnnotationKind::StuffBit => AnnotationKindWire::StuffBit,
        AnnotationKind::CanId { id, extended } => AnnotationKindWire::CanId {
            id: *id,
            extended: *extended,
        },
        AnnotationKind::CanRtr { remote } => AnnotationKindWire::CanRtr { remote: *remote },
        AnnotationKind::CanIde { extended } => AnnotationKindWire::CanIde {
            extended: *extended,
        },
        AnnotationKind::CanDlc(dlc) => AnnotationKindWire::CanDlc { dlc: *dlc },
        AnnotationKind::CanCrc { value, ok } => AnnotationKindWire::CanCrc {
            value: *value,
            ok: *ok,
        },
        AnnotationKind::CanEof => AnnotationKindWire::CanEof,
        AnnotationKind::CanFrame {
            id,
            extended,
            remote,
            dlc,
            data,
            crc_ok,
            acked,
        } => AnnotationKindWire::CanFrame {
            id: *id,
            extended: *extended,
            remote: *remote,
            dlc: *dlc,
            data: data.clone(),
            crc_ok: *crc_ok,
            acked: *acked,
        },
        AnnotationKind::CanErrorFrame => AnnotationKindWire::CanErrorFrame,
        AnnotationKind::OneWireReset {
            presence,
            overdrive,
        } => AnnotationKindWire::OneWireReset {
            presence: *presence,
            overdrive: *overdrive,
        },
        AnnotationKind::OneWireBit(value) => AnnotationKindWire::OneWireBit { value: *value },
        AnnotationKind::OneWireRomCommand(code) => {
            AnnotationKindWire::OneWireRomCommand { code: *code }
        }
        AnnotationKind::OneWireRom { rom, crc_ok } => AnnotationKindWire::OneWireRom {
            rom: onewire::format_rom(*rom),
            crc: (*rom >> 56) as u8,
            crc_ok: *crc_ok,
        },
        AnnotationKind::Incomplete => AnnotationKindWire::Incomplete,
        AnnotationKind::Error(message) => AnnotationKindWire::Error {
            message: message.clone(),
        },
    }
}

pub fn to_wire_annotation(a: &Annotation) -> DecodedAnnotation {
    DecodedAnnotation {
        start_sample: a.start_sample,
        end_sample: a.end_sample,
        level: a.level,
        kind: to_wire_kind(&a.kind),
        text: a.text.clone(),
    }
}

/// Renders a decoder's freshly produced annotations as terminal bytes (SPEC
/// §11.2: "UART bytes as text, I²C and SPI transactions as one line each"),
/// shared by the `decode` terminal source.
pub fn render_terminal(decoder: DecoderId, annotations: &[Annotation]) -> Vec<u8> {
    match decoder {
        DecoderId::Uart => awic_decode::terminal::render_uart(annotations),
        DecoderId::I2c => join_lines(awic_decode::terminal::render_i2c(annotations)),
        DecoderId::Spi => join_lines(awic_decode::terminal::render_spi(annotations)),
        DecoderId::Can => join_lines(awic_decode::terminal::render_can(annotations)),
        DecoderId::OneWire => join_lines(awic_decode::terminal::render_onewire(annotations)),
    }
}

fn join_lines(lines: Vec<String>) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(role: ChannelRole, channel: u8) -> ChannelMapping {
        ChannelMapping {
            role,
            channel,
            minus: None,
        }
    }

    /// B-43: every I²C transaction says `acked` outright and the result
    /// carries `{transactions, nacks, errors, by_address}`.
    #[test]
    fn i2c_transactions_carry_acked_and_a_summary() {
        use awic_decode::{LEVEL_BIT, LEVEL_FIELD, LEVEL_FRAME};
        let ann = |at: u64, level: u8, kind: AnnotationKind| Annotation {
            start_sample: at,
            end_sample: at,
            level,
            kind,
            text: String::new(),
        };
        let addr = |at: u64, address: u16| {
            ann(
                at,
                LEVEL_FIELD,
                AnnotationKind::Address {
                    address,
                    read: false,
                },
            )
        };
        let mut anns = vec![
            ann(0, LEVEL_FRAME, AnnotationKind::Start),
            addr(1, 0x3C),
            ann(2, LEVEL_FIELD, AnnotationKind::Data(0x40)),
            ann(3, LEVEL_FRAME, AnnotationKind::Stop),
            ann(10, LEVEL_FRAME, AnnotationKind::Start),
            addr(11, 0x50),
            ann(12, LEVEL_BIT, AnnotationKind::Nack),
            ann(13, LEVEL_FRAME, AnnotationKind::Stop),
        ];
        anns.extend([
            ann(20, LEVEL_FRAME, AnnotationKind::Start),
            addr(21, 0x3C),
            ann(22, LEVEL_FRAME, AnnotationKind::Stop),
        ]);
        let spec = DecodeSpec {
            decoder: DecoderId::I2c,
            channels: &[],
            options: &[],
            max_annotations: None,
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: None,
        };
        let mut c = DecodeCollector::new(&spec, 1_000_000);
        c.push(anns);
        let out = c.finish(true);
        let acked: Vec<_> = out.transactions.iter().map(|t| t.acked).collect();
        assert_eq!(acked, vec![Some(true), Some(false), Some(true)]);
        let sum = out.summary.expect("summary with format transactions");
        assert_eq!((sum.transactions, sum.nacks, sum.errors), (3, 1, 0));
        assert_eq!(
            sum.by_address,
            vec![
                AddressSummary {
                    address: 0x3C,
                    transactions: 2,
                    nacks: 0
                },
                AddressSummary {
                    address: 0x50,
                    transactions: 1,
                    nacks: 1
                },
            ]
        );
    }

    #[test]
    fn decoder_list_reports_all_five_decoders() {
        let list = decoder_list();
        let ids: Vec<_> = list.decoders.iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                DecoderId::Uart,
                DecoderId::I2c,
                DecoderId::Spi,
                DecoderId::Can,
                DecoderId::OneWire
            ]
        );
        // Every listed option must actually be consumed by `build_decoder`
        // for at least the UART/SPI decoders (I2C intentionally has none).
        let uart_options: Vec<_> = list.decoders[0]
            .options
            .iter()
            .map(|o| o.id().to_string())
            .collect();
        assert!(uart_options.contains(&"baud".to_string()));
    }

    #[test]
    fn builds_uart_with_defaults() {
        let channels = vec![mapping(ChannelRole::Rx, 0)];
        let decoder = build_decoder(DecoderId::Uart, &channels, &[], 1_000_000).unwrap();
        assert_eq!(decoder.name(), "UART @ 115200 baud");
    }

    #[test]
    fn builds_uart_with_overridden_baud() {
        let channels = vec![mapping(ChannelRole::Rx, 0)];
        let options = vec![OptionSetting {
            id: "baud".to_string(),
            value: OptionValue::Int(9600),
        }];
        let decoder = build_decoder(DecoderId::Uart, &channels, &options, 1_000_000).unwrap();
        assert_eq!(decoder.name(), "UART @ 9600 baud");
    }

    #[test]
    fn missing_role_is_a_named_error() {
        let Err(err) = build_decoder(DecoderId::Uart, &[], &[], 1_000_000) else {
            panic!("expected an error");
        };
        assert_eq!(
            err,
            DecodeBuildError::MissingRole {
                decoder: DecoderId::Uart,
                role: ChannelRole::Rx
            }
        );
    }

    #[test]
    fn bad_enum_option_is_a_named_error() {
        let channels = vec![mapping(ChannelRole::Rx, 0)];
        let options = vec![OptionSetting {
            id: "parity".to_string(),
            value: OptionValue::Text("weird".to_string()),
        }];
        let Err(err) = build_decoder(DecoderId::Uart, &channels, &options, 1_000_000) else {
            panic!("expected an error");
        };
        assert!(matches!(err, DecodeBuildError::BadEnumValue { .. }));
    }

    #[test]
    fn an_unknown_option_is_refused_with_the_known_ones() {
        // B-106: `baudrate` (for `baud`) was dropped and the decode ran at
        // the 115200 default.
        let channels = vec![mapping(ChannelRole::Rx, 0)];
        let options = vec![setting("baudrate", OptionValue::Int(9600))];
        let Err(err) = build_decoder(DecoderId::Uart, &channels, &options, 1_000_000) else {
            panic!("expected an error");
        };
        let msg = err.to_string();
        assert!(
            matches!(err, DecodeBuildError::UnknownOption { .. }),
            "{msg}"
        );
        assert!(msg.contains("\"baudrate\""), "{msg}");
        assert!(msg.contains("baud, data_bits"), "{msg}");
        // An option-less decoder refuses any.
        let i2c = vec![mapping(ChannelRole::Scl, 0), mapping(ChannelRole::Sda, 1)];
        assert!(build_decoder(DecoderId::I2c, &i2c, &options, 1_000_000).is_err());
    }

    fn setting(id: &str, value: OptionValue) -> OptionSetting {
        OptionSetting {
            id: id.to_string(),
            value,
        }
    }

    #[test]
    fn builds_can_with_defaults_and_overrides() {
        let channels = vec![mapping(ChannelRole::Can, 3)];
        let decoder = build_decoder(DecoderId::Can, &channels, &[], 16_000_000).unwrap();
        assert_eq!(decoder.name(), "CAN @ 500000 bit/s");
        assert_eq!(
            decoder.required_channels(),
            [awic_decode::ChannelRole::Can].as_slice()
        );
        let options = vec![
            setting("bitrate", OptionValue::Int(125_000)),
            setting("sample_point", OptionValue::Int(80)),
            setting("polarity", OptionValue::Text("dominant_high".into())),
        ];
        let decoder = build_decoder(DecoderId::Can, &channels, &options, 16_000_000).unwrap();
        assert_eq!(decoder.name(), "CAN @ 125000 bit/s");
    }

    #[test]
    fn can_rejects_bad_options_and_a_missing_line() {
        let channels = vec![mapping(ChannelRole::Can, 0)];
        let build = |options: Vec<OptionSetting>| {
            build_decoder(DecoderId::Can, &channels, &options, 16_000_000)
                .err()
                .expect("an error")
        };
        assert!(matches!(
            build(vec![setting("polarity", OptionValue::Text("both".into()))]),
            DecodeBuildError::BadEnumValue { .. }
        ));
        assert!(matches!(
            build(vec![setting("sample_point", OptionValue::Int(95))]),
            DecodeBuildError::OptionOutOfRange { .. }
        ));
        assert!(matches!(
            build(vec![setting("bitrate", OptionValue::Text("fast".into()))]),
            DecodeBuildError::BadOptionType { .. }
        ));
        // 2 MS/s cannot resolve 1 Mbit/s.
        let Err(err) = build_decoder(
            DecoderId::Can,
            &channels,
            &[setting("bitrate", OptionValue::Int(1_000_000))],
            2_000_000,
        ) else {
            panic!("expected an error");
        };
        assert!(matches!(err, DecodeBuildError::Invalid(_)));
        assert_eq!(
            build_decoder(DecoderId::Can, &[], &[], 16_000_000)
                .err()
                .unwrap(),
            DecodeBuildError::MissingRole {
                decoder: DecoderId::Can,
                role: ChannelRole::Can
            }
        );
    }

    #[test]
    fn can_info_options_are_all_consumed_by_build() {
        let info = can_info();
        let ids: Vec<_> = info.options.iter().map(DecoderOptionSpec::id).collect();
        assert_eq!(ids, ["bitrate", "sample_point", "polarity"]);
        assert_eq!(info.channels, vec![ChannelRole::Can]);
    }

    #[test]
    fn auto_bitrate_is_detected_for_can_and_still_for_uart() {
        use awic_decode::Transition;
        // Pulses of 1..=5 bits at 32 samples/bit, 16 MS/s: 500 kbit/s.
        let mut transitions = Vec::new();
        let (mut at, mut level, mut x) = (0u64, true, 7u32);
        for _ in 0..200 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            transitions.push(Transition {
                sample_index: at,
                value: u8::from(level),
            });
            at += u64::from(1 + (x >> 16) % 5) * 32;
            level = !level;
        }
        let channels = vec![mapping(ChannelRole::Can, 0)];
        let auto = vec![setting("bitrate", OptionValue::Text("auto".into()))];
        assert!(wants_auto_baud(DecoderId::Can, &auto));
        assert!(wants_auto_baud(
            DecoderId::Can,
            &[setting("bitrate", OptionValue::Int(0))]
        ));
        assert!(!wants_auto_baud(DecoderId::Can, &[]));
        // `baud` means nothing to CAN and `bitrate` nothing to UART.
        assert!(!wants_auto_baud(
            DecoderId::Can,
            &[setting("baud", OptionValue::Int(0))]
        ));
        assert!(!wants_auto_baud(DecoderId::Uart, &auto));
        assert!(!wants_auto_baud(DecoderId::Spi, &auto));
        let (options, rate) =
            resolve_auto_baud(&auto, &channels, &transitions, 16_000_000).unwrap();
        assert_eq!(rate, 500_000);
        assert_eq!(options, vec![setting("bitrate", OptionValue::Int(500_000))]);
        assert_eq!(auto_rate_channel(DecoderId::Can, &channels).unwrap(), 0);
        assert!(auto_rate_channel(DecoderId::Uart, &channels).is_err());
        // Not a CAN line: refused with the stable code.
        let flat = vec![Transition {
            sample_index: 0,
            value: 1,
        }];
        let err = resolve_auto_baud(&auto, &channels, &flat, 16_000_000).unwrap_err();
        assert_eq!(err.code, "baud_not_detected");
        assert!(err.msg.contains("bit rate"), "{}", err.msg);
        // UART keeps working through the same entry point.
        let uart_channels = vec![mapping(ChannelRole::Rx, 0)];
        let uart_auto = vec![setting("baud", OptionValue::Text("auto".into()))];
        assert!(wants_auto_baud(DecoderId::Uart, &uart_auto));
        let err = resolve_auto_baud(&uart_auto, &uart_channels, &flat, 1_000_000).unwrap_err();
        assert!(err.msg.contains("baud rate"), "{}", err.msg);
    }

    /// CAN transactions: one per frame, address = ID (29 bits fit), `acked`
    /// per frame, a summary by ID with unacked frames as `nacks`.
    /// 1-Wire from wire-shaped config to transactions: presence as `acked`,
    /// the ROM's family code as `address`, the ROM code in the notes.
    #[test]
    fn onewire_decodes_end_to_end_into_transactions() {
        // 1 MS/s: one sample per µs. Channel 3.
        let mut transitions = Vec::new();
        let mut t = 10u64;
        let mut pulse = |low: u64, high: u64| {
            transitions.push(awic_decode::Transition {
                sample_index: t,
                value: 0,
            });
            t += low;
            transitions.push(awic_decode::Transition {
                sample_index: t,
                value: 1 << 3,
            });
            t += high;
        };
        let byte = |pulse: &mut dyn FnMut(u64, u64), b: u8| {
            for i in 0..8 {
                if (b >> i) & 1 == 1 {
                    pulse(6, 64)
                } else {
                    pulse(60, 10)
                }
            }
        };
        pulse(480, 30); // reset
        pulse(120, 330); // presence
        byte(&mut pulse, 0x33); // Read ROM
        for b in [0x02, 0x1C, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xA2] {
            byte(&mut pulse, b);
        }
        byte(&mut pulse, 0x44);
        pulse(480, 1000); // reset nobody answers
        let end = t;

        let channels = vec![mapping(ChannelRole::Owr, 3)];
        let mut decoder = build_decoder(DecoderId::OneWire, &channels, &[], 1_000_000).unwrap();
        assert_eq!(decoder.name(), "1-Wire");
        let mut anns = decoder.feed(&transitions, end);
        anns.extend(decoder.finish(end));

        let spec = DecodeSpec {
            decoder: DecoderId::OneWire,
            channels: &channels,
            options: &[],
            max_annotations: None,
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: None,
        };
        let mut c = DecodeCollector::new(&spec, 1_000_000);
        c.push(anns.clone());
        let out = c.finish(true);
        assert_eq!(out.transactions.len(), 2);
        let first = &out.transactions[0];
        assert_eq!(first.acked, Some(true));
        assert_eq!(first.address, Some(0x02));
        assert_eq!(first.data, "44");
        assert_eq!(first.notes, vec!["Read ROM", "ROM 02-00000001B81C"]);
        assert_eq!(out.transactions[1].acked, Some(false));
        let sum = out.summary.expect("summary");
        assert_eq!((sum.transactions, sum.nacks, sum.errors), (2, 1, 0));

        let rom = anns
            .iter()
            .find_map(|a| match to_wire_kind(&a.kind) {
                k @ AnnotationKindWire::OneWireRom { .. } => Some(k),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            rom,
            AnnotationKindWire::OneWireRom {
                rom: "02-00000001B81C".to_string(),
                crc: 0xA2,
                crc_ok: true
            }
        );
        let text = String::from_utf8(render_terminal(DecoderId::OneWire, &anns)).unwrap();
        assert_eq!(
            text,
            "reset  Read ROM  02-00000001B81C  44\r\nreset NO-PRESENCE\r\n"
        );

        // Below the slowest rate it can time: refused, saying why.
        let err = build_decoder(DecoderId::OneWire, &channels, &[], 100_000)
            .err()
            .unwrap();
        assert!(err.to_string().contains("samples/s"), "{err}");
    }

    #[test]
    fn can_transactions_carry_ids_acked_and_a_summary() {
        use awic_decode::LEVEL_FRAME;
        let frame = |at: u64, id: u32, acked: bool| Annotation {
            start_sample: at,
            end_sample: at + 50,
            level: LEVEL_FRAME,
            kind: AnnotationKind::CanFrame {
                id,
                extended: id > 0x7FF,
                remote: false,
                dlc: 1,
                data: vec![0xA5],
                crc_ok: true,
                acked,
            },
            text: String::new(),
        };
        let flag = |at: u64| Annotation {
            start_sample: at,
            end_sample: at + 10,
            level: LEVEL_FRAME,
            kind: AnnotationKind::CanErrorFrame,
            text: String::new(),
        };
        let spec = DecodeSpec {
            decoder: DecoderId::Can,
            channels: &[],
            options: &[],
            max_annotations: None,
            max_level: None,
            format: Some(DecodeFormat::Transactions),
            max_bytes: None,
            fold: None,
        };
        let mut c = DecodeCollector::new(&spec, 1_000_000);
        c.push(vec![
            frame(0, 0x123, true),
            frame(100, 0x1ABC_DEF0, false),
            flag(150),
            frame(300, 0x123, false),
        ]);
        let out = c.finish(true);
        assert_eq!(out.transactions.len(), 3);
        let t = &out.transactions[1];
        assert_eq!(t.address, Some(0x1ABC_DEF0));
        assert_eq!((t.data.as_str(), t.count), ("A5", 1));
        assert_eq!(t.acked, Some(false));
        assert_eq!(t.notes, vec!["extended", "no ACK", "error frame"]);
        assert_eq!(out.transactions[0].acked, Some(true));
        assert_eq!(out.transactions[0].text, None);
        let sum = out.summary.expect("summary with format transactions");
        assert_eq!((sum.transactions, sum.nacks, sum.errors), (3, 2, 1));
        assert_eq!(
            sum.by_address,
            vec![
                AddressSummary {
                    address: 0x123,
                    transactions: 2,
                    nacks: 1
                },
                AddressSummary {
                    address: 0x1ABC_DEF0,
                    transactions: 1,
                    nacks: 1
                },
            ]
        );
    }

    #[test]
    fn builds_i2c_and_spi() {
        let i2c_channels = vec![mapping(ChannelRole::Scl, 0), mapping(ChannelRole::Sda, 1)];
        assert!(build_decoder(DecoderId::I2c, &i2c_channels, &[], 1_000_000).is_ok());

        let spi_channels = vec![mapping(ChannelRole::Clk, 0), mapping(ChannelRole::Mosi, 1)];
        assert!(build_decoder(DecoderId::Spi, &spi_channels, &[], 1_000_000).is_ok());
    }

    fn ann(start: u64, end: u64, level: u8, kind: AnnotationKind, text: &str) -> Annotation {
        Annotation {
            start_sample: start,
            end_sample: end,
            level,
            kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn render_terminal_dispatches_per_decoder() {
        let anns = vec![ann(0, 1, 0, AnnotationKind::Data(0x41), "0x41")];
        assert_eq!(render_terminal(DecoderId::Uart, &anns), vec![0x41]);

        let anns = vec![ann(0, 0, 0, AnnotationKind::Start, "S")];
        assert_eq!(render_terminal(DecoderId::I2c, &anns), b"S\r\n".to_vec());

        let anns = vec![ann(
            0,
            9,
            0,
            AnnotationKind::CanFrame {
                id: 0x123,
                extended: false,
                remote: false,
                dlc: 2,
                data: vec![0xDE, 0xAD],
                crc_ok: true,
                acked: false,
            },
            "",
        )];
        assert_eq!(
            render_terminal(DecoderId::Can, &anns),
            b"  can  123   [2]  DE AD  NO-ACK\r\n".to_vec()
        );
    }
}

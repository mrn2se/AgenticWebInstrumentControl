//! Recursive-descent parser: pattern text → [`Pattern`] (SPEC §2.9.1).
//!
//! Grammar, as implemented (case-insensitive keywords; `x`/`X` don't-care):
//!
//! ```text
//! pattern      := [channels] clause           (* channel optional with one digital channel *)
//! channels     := "D" digit                    (* single digital *)
//!               | "CH" digit                    (* single analog *)
//!               | "D" digit ".." "D" digit      (* MSB..LSB range *)
//!               | "D" digit ("," "D" digit)+    (* explicit list, MSB first *)
//!
//! clause       := "=" bits                                  (* state, range/list channels only *)
//!               | "f=" freq [","] bits ["tol=" float]        (* bit sequence *)
//!               | "clk=" "D" digit edge_dir bits              (* clocked *)
//!               | edge_dir                                    (* bare edge, digital *)
//!               | level bound [bound]                         (* pulse, digital *)
//!               | analog_dir voltage [hyst]                   (* analog edge *)
//!               | cmp voltage [hyst]                          (* analog level *)
//!
//! edge_dir     := "rising" | "falling" | "edge"
//! analog_dir   := "rising" | "falling" | "cross"
//! level        := "high" | "low"
//! bound        := cmp duration
//! cmp          := ">=" | "<=" | ">" | "<"
//! hyst         := "hyst=" voltage
//! bits         := ("0" | "1" | "x" | "X")+
//! freq         := number ["k" | "K" | "m" | "M"]                (* Hz *)
//! duration     := number ("ns" | "us" | "µs" | "ms" | "s")
//! voltage      := number ("V" | "v")
//! ```
//!
//! Tokens are whitespace-separated; a trailing `,` on a token is stripped
//! (SPEC §2.9.1: "commas optional").

use crate::error::{ParseError, Span};
use crate::pattern::{
    AnalogPattern, BitSeqPattern, BitVal, ClockedPattern, Cmp, EdgeDir, EdgePattern, FireOn,
    Pattern, PulsePattern, StatePattern,
};
use crate::types::{ChannelInfo, ChannelKind, RecordingInfo};

const DEFAULT_TOL_CELLS: f64 = 0.3;
/// Hysteresis default when `hyst=` is omitted: a few raw ADC codes, so
/// noise near the threshold does not produce a burst of crossings (SPEC
/// §2.9.1 task note). Expressed in codes, independent of the channel's
/// volts/code scale.
const DEFAULT_HYST_CODES: f64 = 3.0;

#[derive(Clone, Copy)]
struct Tok<'a> {
    text: &'a str,
    start: usize,
}

impl<'a> Tok<'a> {
    fn span(&self) -> Span {
        Span::new(self.start, self.start + self.text.len())
    }
}

fn tokenize(text: &str) -> Vec<Tok<'_>> {
    let mut toks = Vec::new();
    let mut cur_start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = cur_start.take() {
                toks.push(raw_tok(text, s, i));
            }
        } else if cur_start.is_none() {
            cur_start = Some(i);
        }
    }
    if let Some(s) = cur_start {
        toks.push(raw_tok(text, s, text.len()));
    }
    toks
}

/// Builds a token from `text[start..end]`, stripping one trailing `,`
/// (SPEC §2.9.1: commas are an optional separator, not part of any token).
fn raw_tok(text: &str, start: usize, end: usize) -> Tok<'_> {
    let mut s = &text[start..end];
    if let Some(stripped) = s.strip_suffix(',') {
        s = stripped;
    }
    Tok { text: s, start }
}

/// Parses one pattern line against the given recording (SPEC §2.9.1).
///
/// `channels` lists every channel the recording has; a name the pattern
/// uses that is not in this list is refused with the list of names that
/// *are* valid, per SPEC §2.9.1's requirement that a parse error be
/// actionable from the message alone.
pub fn parse(
    text: &str,
    channels: &[ChannelInfo],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let toks: Vec<Tok<'_>> = tokenize(text)
        .into_iter()
        .filter(|t| !t.text.is_empty())
        .collect();
    if toks.is_empty() {
        return Err(ParseError::new(Span::at(0), "empty pattern"));
    }

    let head = toks[0];
    let rest = &toks[1..];

    if head.text.contains("..") {
        return parse_multi_range(head, rest, channels);
    }
    if head.text.contains(',') {
        return parse_multi_list(head, rest, channels);
    }
    if !looks_like_channel(head.text) {
        // `f=1000, 100010x101` — the form the user first wrote. With one
        // digital channel there is nothing to disambiguate; with several,
        // guessing would search the wrong line, so ask instead.
        let ch = sole_digital_channel(channels, head.span())?;
        let implicit = Tok {
            text: "",
            start: head.start,
        };
        return parse_single_clause(ch, implicit, &toks, channels, rec);
    }
    parse_single(head, rest, channels, rec)
}

/// `D<n>` or `CH<n>` — anything else in first position is a clause.
fn looks_like_channel(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let digits = lower.strip_prefix("ch").or_else(|| lower.strip_prefix('d'));
    digits.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

fn sole_digital_channel(channels: &[ChannelInfo], span: Span) -> Result<&ChannelInfo, ParseError> {
    let mut digital = channels.iter().filter(|c| c.kind_is_digital());
    match (digital.next(), digital.next()) {
        (Some(only), None) => Ok(only),
        _ => {
            let names: Vec<&str> = channels.iter().map(|c| c.name.as_str()).collect();
            Err(ParseError::new(
                span,
                format!(
                    "name the channel first, e.g. \"D0 {}\"; this recording has: {}",
                    "f=1000 1010",
                    if names.is_empty() {
                        "no channels".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
            ))
        }
    }
}

// ---------------------------------------------------------------------
// Channel resolution
// ---------------------------------------------------------------------

fn find_channel<'c>(
    name: &str,
    span: Span,
    channels: &'c [ChannelInfo],
) -> Result<&'c ChannelInfo, ParseError> {
    channels
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let have = if channels.is_empty() {
                "this recording has no channels".to_string()
            } else {
                let names: Vec<&str> = channels.iter().map(|c| c.name.as_str()).collect();
                format!("this recording has: {}", names.join(", "))
            };
            ParseError::new(span, format!("unknown channel {name:?}; {have}"))
        })
}

fn digital_bit(ch: &ChannelInfo, span: Span) -> Result<u8, ParseError> {
    match ch.kind {
        ChannelKind::Digital { bit } => Ok(bit),
        ChannelKind::Analog { .. } => Err(ParseError::new(
            span,
            format!(
                "{:?} is an analog channel; this clause needs a digital channel (D0..D7)",
                ch.name
            ),
        )),
    }
}

// ---------------------------------------------------------------------
// Multi-channel state: `D3..D0 = 1x01` / `D0,D2 = 10`
// ---------------------------------------------------------------------

fn parse_multi_range(
    head: Tok<'_>,
    rest: &[Tok<'_>],
    channels: &[ChannelInfo],
) -> Result<Pattern, ParseError> {
    let Some((hi_txt, lo_txt)) = head.text.split_once("..") else {
        unreachable!("caller checked for \"..\"");
    };
    let hi_span = Span::new(head.start, head.start + hi_txt.len());
    let lo_span = Span::new(head.start + hi_txt.len() + 2, head.start + head.text.len());
    let hi = digital_bit(find_channel(hi_txt, hi_span, channels)?, hi_span)?;
    let lo = digital_bit(find_channel(lo_txt, lo_span, channels)?, lo_span)?;
    if hi < lo {
        return Err(ParseError::new(
            head.span(),
            format!(
                "channel range must be MSB..LSB, e.g. D{hi}..D{lo} reversed — try D{lo}..D{hi}"
            ),
        ));
    }
    let bits_len = (hi - lo + 1) as usize;
    let order: Vec<u8> = (lo..=hi).rev().collect(); // MSB first
    let bits_tok = expect_state_bits(rest, head, bits_len)?;
    let bits = parse_bits(bits_tok)?;
    build_state_pattern(&order, &bits, bits_tok)
}

fn parse_multi_list(
    head: Tok<'_>,
    rest: &[Tok<'_>],
    channels: &[ChannelInfo],
) -> Result<Pattern, ParseError> {
    let mut order = Vec::new();
    let mut offset = head.start;
    for part in head.text.split(',') {
        let span = Span::new(offset, offset + part.len());
        let bit = digital_bit(find_channel(part, span, channels)?, span)?;
        order.push(bit);
        offset += part.len() + 1; // +1 for the comma
    }
    let bits_tok = expect_state_bits(rest, head, order.len())?;
    let bits = parse_bits(bits_tok)?;
    build_state_pattern(&order, &bits, bits_tok)
}

fn expect_state_bits<'a>(
    rest: &[Tok<'a>],
    head: Tok<'_>,
    expected_len: usize,
) -> Result<Tok<'a>, ParseError> {
    if rest.first().map(|t| t.text) != Some("=") {
        return Err(ParseError::new(
            Span::at(head.start + head.text.len()),
            "expected '=' followed by a bit pattern, e.g. D3..D0 = 1x01",
        ));
    }
    match rest.get(1) {
        Some(t) if rest.len() == 2 => {
            if t.text.chars().count() != expected_len {
                return Err(ParseError::new(
                    t.span(),
                    format!(
                        "expected {expected_len} bit(s) (one per channel), found {} in {:?}",
                        t.text.chars().count(),
                        t.text
                    ),
                ));
            }
            Ok(*t)
        }
        Some(t) => Err(ParseError::new(
            t.span(),
            "unexpected trailing tokens after the bit pattern",
        )),
        None => Err(ParseError::new(
            Span::at(rest[0].start + rest[0].text.len()),
            "expected a bit pattern after '=', e.g. 1x01",
        )),
    }
}

fn build_state_pattern(
    order: &[u8],
    bits: &[BitVal],
    bits_tok: Tok<'_>,
) -> Result<Pattern, ParseError> {
    let mut mask = 0u8;
    let mut expected = 0u8;
    for (&bit, &val) in order.iter().zip(bits.iter()) {
        match val {
            BitVal::X => {}
            BitVal::One => {
                mask |= 1 << bit;
                expected |= 1 << bit;
            }
            BitVal::Zero => {
                mask |= 1 << bit;
            }
        }
    }
    if mask == 0 {
        return Err(ParseError::new(
            bits_tok.span(),
            "a state pattern of all 'x' matches nothing; give at least one 0 or 1",
        ));
    }
    Ok(Pattern::State(StatePattern { mask, expected }))
}

// ---------------------------------------------------------------------
// Single-channel forms
// ---------------------------------------------------------------------

fn parse_single(
    head: Tok<'_>,
    rest: &[Tok<'_>],
    channels: &[ChannelInfo],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let ch = find_channel(head.text, head.span(), channels)?;
    parse_single_clause(ch, head, rest, channels, rec)
}

/// The clause after a resolved channel; `head` is the channel token (empty
/// when the channel was implicit) and only positions error messages.
fn parse_single_clause(
    ch: &ChannelInfo,
    head: Tok<'_>,
    rest: &[Tok<'_>],
    channels: &[ChannelInfo],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let Some(&first) = rest.first() else {
        return Err(ParseError::new(
            Span::at(head.start + head.text.len()),
            "expected a clause after the channel: f=, clk=, rising/falling/edge, high/low, or a comparison like >1.5V",
        ));
    };
    let kv = split_kv(first.text);

    match kv.map(|(k, _)| k.to_ascii_lowercase()).as_deref() {
        Some("f") => parse_bitseq(ch, head, first, &rest[1..], rec),
        Some("clk") => parse_clocked(ch, head, first, kv.unwrap().1, &rest[1..], channels),
        _ => match first.text.to_ascii_lowercase().as_str() {
            "rising" | "falling" | "edge" if ch.kind_is_digital() => {
                parse_edge_or_pulse_digital(ch, head, first, &rest[1..])
            }
            "high" | "low" => parse_pulse(ch, head, first, &rest[1..], rec),
            "rising" | "falling" | "cross" => parse_analog_edge(ch, head, first, &rest[1..], rec),
            "edge" => Err(ParseError::new(
                first.span(),
                "\"edge\" is a digital-only clause; analog channels use rising, falling or cross",
            )),
            _ if starts_with_cmp(first.text) => parse_analog_level(ch, head, first, &rest[1..], rec),
            other => Err(ParseError::new(
                first.span(),
                format!(
                    "unrecognized clause {other:?}; expected f=, clk=, rising/falling/edge, high/low, or a comparison like >1.5V"
                ),
            )),
        },
    }
}

impl ChannelInfo {
    fn kind_is_digital(&self) -> bool {
        matches!(self.kind, ChannelKind::Digital { .. })
    }
}

fn parse_edge_or_pulse_digital(
    ch: &ChannelInfo,
    head: Tok<'_>,
    first: Tok<'_>,
    trailing: &[Tok<'_>],
) -> Result<Pattern, ParseError> {
    if let Some(extra) = trailing.first() {
        return Err(ParseError::new(
            extra.span(),
            "unexpected trailing tokens after a bare edge; rising/falling/edge take no level",
        ));
    }
    let bit = digital_bit(ch, head.span())?;
    let dir = match first.text.to_ascii_lowercase().as_str() {
        "rising" => EdgeDir::Rising,
        "falling" => EdgeDir::Falling,
        "edge" => EdgeDir::Either,
        _ => unreachable!(),
    };
    Ok(Pattern::Edge(EdgePattern { bit, dir }))
}

fn parse_bitseq(
    ch: &ChannelInfo,
    head: Tok<'_>,
    f_tok: Tok<'_>,
    rest: &[Tok<'_>],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let bit = digital_bit(ch, head.span())?;
    let (_, f_val) = split_kv(f_tok.text).unwrap();
    let f_span = Span::new(f_tok.start + 2, f_tok.start + f_tok.text.len());
    let freq = parse_frequency(f_val).map_err(|m| ParseError::new(f_span, m))?;

    let nyquist = rec.rate_hz / 2.0;
    if freq <= 0.0 || freq > nyquist {
        return Err(ParseError::new(
            f_span,
            format!(
                "f={f_val} ({freq:.3} Hz) cannot be resolved at {}Hz: must be > 0 and \u{2264} {:.3} Hz (rate/2)",
                rec.rate_hz, nyquist
            ),
        ));
    }

    let mut tol_cells = DEFAULT_TOL_CELLS;
    let mut bits_tok: Option<Tok<'_>> = None;
    for t in rest {
        if let Some((k, v)) = split_kv(t.text) {
            if k.eq_ignore_ascii_case("tol") {
                let v_span = Span::new(t.start + k.len() + 1, t.start + t.text.len());
                let tol: f64 = v.parse().map_err(|_| {
                    ParseError::new(
                        v_span,
                        format!(
                            "invalid tol value {v:?}; expected a number of cells, e.g. tol=0.3"
                        ),
                    )
                })?;
                if !(tol > 0.0 && tol <= 0.5) {
                    return Err(ParseError::new(
                        v_span,
                        format!("tol={tol} out of range; must be > 0 and \u{2264} 0.5 cells"),
                    ));
                }
                tol_cells = tol;
                continue;
            }
            return Err(ParseError::new(
                t.span(),
                format!("unexpected option {k:?}; only tol= is accepted here"),
            ));
        }
        if bits_tok.is_some() {
            return Err(ParseError::new(
                t.span(),
                "unexpected trailing tokens after the bit pattern",
            ));
        }
        bits_tok = Some(*t);
    }
    let Some(bits_tok) = bits_tok else {
        return Err(ParseError::new(
            Span::at(f_tok.start + f_tok.text.len()),
            "expected a bit pattern after f=, e.g. f=1k 100010x101",
        ));
    };
    let bits = parse_bits(bits_tok)?;
    if bits.is_empty() {
        return Err(ParseError::new(
            bits_tok.span(),
            "bit pattern must not be empty",
        ));
    }

    Ok(Pattern::BitSeq(BitSeqPattern {
        bit,
        cell_samples: rec.rate_hz / freq,
        tol_cells,
        bits,
    }))
}

fn parse_clocked(
    ch: &ChannelInfo,
    head: Tok<'_>,
    clk_tok: Tok<'_>,
    clk_name: &str,
    rest: &[Tok<'_>],
    channels: &[ChannelInfo],
) -> Result<Pattern, ParseError> {
    let data_bit = digital_bit(ch, head.span())?;
    let clk_span = Span::new(clk_tok.start + 4, clk_tok.start + clk_tok.text.len());
    let clk_ch = find_channel(clk_name, clk_span, channels)?;
    let clk_bit = digital_bit(clk_ch, clk_span)?;
    if clk_bit == data_bit {
        return Err(ParseError::new(
            clk_span,
            format!(
                "clock channel {clk_name:?} must differ from the data channel {:?}",
                ch.name
            ),
        ));
    }

    let Some(&dir_tok) = rest.first() else {
        return Err(ParseError::new(
            Span::at(clk_tok.start + clk_tok.text.len()),
            "expected rising or falling after clk=, e.g. clk=D0 rising 1010x",
        ));
    };
    let clk_dir = match dir_tok.text.to_ascii_lowercase().as_str() {
        "rising" => EdgeDir::Rising,
        "falling" => EdgeDir::Falling,
        other => {
            return Err(ParseError::new(
                dir_tok.span(),
                format!("expected rising or falling, found {other:?} (a clocked pattern needs one edge direction, not 'edge')"),
            ))
        }
    };

    let Some(&bits_tok) = rest.get(1) else {
        return Err(ParseError::new(
            Span::at(dir_tok.start + dir_tok.text.len()),
            "expected a bit pattern after the clock edge, e.g. clk=D0 rising 1010x",
        ));
    };
    if rest.len() > 2 {
        return Err(ParseError::new(
            rest[2].span(),
            "unexpected trailing tokens after the bit pattern",
        ));
    }
    let bits = parse_bits(bits_tok)?;
    if bits.is_empty() {
        return Err(ParseError::new(
            bits_tok.span(),
            "bit pattern must not be empty",
        ));
    }

    Ok(Pattern::Clocked(ClockedPattern {
        data_bit,
        clk_bit,
        clk_dir,
        bits,
    }))
}

fn parse_pulse(
    ch: &ChannelInfo,
    head: Tok<'_>,
    level_tok: Tok<'_>,
    rest: &[Tok<'_>],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let bit = digital_bit(ch, head.span())?;
    let high = level_tok.text.eq_ignore_ascii_case("high");

    if rest.is_empty() || rest.len() > 2 {
        return Err(ParseError::new(
            level_tok.span(),
            "expected one or two duration bounds after high/low, e.g. high >2ms <5ms, or low >=10ms",
        ));
    }

    let mut min: Option<(Cmp, f64)> = None;
    let mut max: Option<(Cmp, f64)> = None;
    for t in rest {
        let (cmp, num_txt) = parse_cmp_prefix(t.text).map_err(|m| ParseError::new(t.span(), m))?;
        let num_span = Span::new(
            t.start + (t.text.len() - num_txt.len()),
            t.start + t.text.len(),
        );
        let secs = parse_duration(num_txt).map_err(|m| ParseError::new(num_span, m))?;
        let samples = secs * rec.rate_hz;
        if cmp.is_lower() {
            if min.is_some() {
                return Err(ParseError::new(
                    t.span(),
                    "two lower bounds given; expected one lower (>,>=) and one upper (<,<=)",
                ));
            }
            min = Some((cmp, samples));
        } else {
            if max.is_some() {
                return Err(ParseError::new(
                    t.span(),
                    "two upper bounds given; expected one lower (>,>=) and one upper (<,<=)",
                ));
            }
            max = Some((cmp, samples));
        }
    }

    Ok(Pattern::Pulse(PulsePattern {
        bit,
        high,
        min,
        max,
    }))
}

fn parse_analog_edge(
    ch: &ChannelInfo,
    head: Tok<'_>,
    dir_tok: Tok<'_>,
    rest: &[Tok<'_>],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let cal = analog_calibration(ch, head.span())?;
    let fire_on = match dir_tok.text.to_ascii_lowercase().as_str() {
        "rising" => FireOn::Rising,
        "falling" => FireOn::Falling,
        "cross" => FireOn::Both,
        _ => unreachable!(),
    };
    let Some(&v_tok) = rest.first() else {
        return Err(ParseError::new(
            Span::at(dir_tok.start + dir_tok.text.len()),
            "expected a voltage, e.g. CH1 rising 1.2V",
        ));
    };
    let volts = parse_voltage(v_tok.text).map_err(|m| ParseError::new(v_tok.span(), m))?;
    let hyst_v = parse_optional_hyst(rest.get(1))?;
    finish_analog(cal, volts, hyst_v, fire_on, rest.get(2), rec)
}

fn parse_analog_level(
    ch: &ChannelInfo,
    head: Tok<'_>,
    cmp_tok: Tok<'_>,
    rest: &[Tok<'_>],
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    let cal = analog_calibration(ch, head.span())?;
    let (cmp, num_txt) =
        parse_cmp_prefix(cmp_tok.text).map_err(|m| ParseError::new(cmp_tok.span(), m))?;
    let num_span = Span::new(
        cmp_tok.start + (cmp_tok.text.len() - num_txt.len()),
        cmp_tok.start + cmp_tok.text.len(),
    );
    let volts = parse_voltage(num_txt).map_err(|m| ParseError::new(num_span, m))?;
    let fire_on = if cmp.is_lower() {
        FireOn::Rising
    } else {
        FireOn::Falling
    };
    let hyst_v = parse_optional_hyst(rest.first())?;
    finish_analog(cal, volts, hyst_v, fire_on, rest.get(1), rec)
}

fn analog_calibration(ch: &ChannelInfo, span: Span) -> Result<(u8, f32, f32), ParseError> {
    match ch.kind {
        ChannelKind::Analog {
            index,
            scale,
            offset,
        } => Ok((index, scale, offset)),
        ChannelKind::Digital { .. } => Err(ParseError::new(
            span,
            format!(
                "{:?} is a digital channel; this clause needs an analog channel (CH1, CH2, ...)",
                ch.name
            ),
        )),
    }
}

fn parse_optional_hyst(tok: Option<&Tok<'_>>) -> Result<Option<f64>, ParseError> {
    let Some(t) = tok else { return Ok(None) };
    let Some((k, v)) = split_kv(t.text) else {
        return Err(ParseError::new(
            t.span(),
            format!(
                "unexpected trailing token {:?}; expected hyst=, e.g. hyst=0.05V",
                t.text
            ),
        ));
    };
    if !k.eq_ignore_ascii_case("hyst") {
        return Err(ParseError::new(
            t.span(),
            format!("unexpected option {k:?}; only hyst= is accepted here"),
        ));
    }
    let v_span = Span::new(t.start + k.len() + 1, t.start + t.text.len());
    let volts = parse_voltage(v).map_err(|m| ParseError::new(v_span, m))?;
    Ok(Some(volts))
}

fn finish_analog(
    (index, scale, offset): (u8, f32, f32),
    threshold_v: f64,
    hyst_v: Option<f64>,
    fire_on: FireOn,
    extra: Option<&Tok<'_>>,
    rec: RecordingInfo,
) -> Result<Pattern, ParseError> {
    if let Some(t) = extra {
        return Err(ParseError::new(t.span(), "unexpected trailing tokens"));
    }
    let scale = scale as f64;
    if scale == 0.0 {
        return Err(ParseError::new(
            Span::at(0),
            "channel calibration has scale=0; cannot convert volts to a raw code",
        ));
    }
    let threshold_code = (threshold_v - offset as f64) / scale;
    // Hysteresis is a *width* in volts; converting it to codes must not
    // flip sign even when `scale` is negative (an inverted channel).
    let hyst_code = match hyst_v {
        Some(v) => (v / scale).abs(),
        None => DEFAULT_HYST_CODES,
    };
    Ok(Pattern::Analog(AnalogPattern {
        channel_index: index,
        threshold_code,
        hyst_code,
        fire_on,
        sample_bits: rec.sample_bits,
        channel_count: rec.channel_count,
    }))
}

// ---------------------------------------------------------------------
// Small token-level parsers
// ---------------------------------------------------------------------

fn split_kv(s: &str) -> Option<(&str, &str)> {
    let (k, v) = s.split_once('=')?;
    if k.is_empty() || v.is_empty() {
        return None;
    }
    Some((k, v))
}

fn starts_with_cmp(s: &str) -> bool {
    s.starts_with('>') || s.starts_with('<')
}

fn parse_cmp_prefix(s: &str) -> Result<(Cmp, &str), String> {
    if let Some(rest) = s.strip_prefix(">=") {
        Ok((Cmp::Ge, rest))
    } else if let Some(rest) = s.strip_prefix("<=") {
        Ok((Cmp::Le, rest))
    } else if let Some(rest) = s.strip_prefix('>') {
        Ok((Cmp::Gt, rest))
    } else if let Some(rest) = s.strip_prefix('<') {
        Ok((Cmp::Lt, rest))
    } else {
        Err(format!(
            "expected a comparison starting with >, >=, < or <=, found {s:?}"
        ))
    }
}

fn parse_bits(tok: Tok<'_>) -> Result<Vec<BitVal>, ParseError> {
    let mut out = Vec::with_capacity(tok.text.len());
    for (i, c) in tok.text.char_indices() {
        let v = match c {
            '0' => BitVal::Zero,
            '1' => BitVal::One,
            'x' | 'X' => BitVal::X,
            _ => {
                return Err(ParseError::new(
                    Span::new(tok.start + i, tok.start + i + c.len_utf8()),
                    format!("invalid bit character {c:?}; only 0, 1 and x are allowed"),
                ))
            }
        };
        out.push(v);
    }
    Ok(out)
}

fn parse_frequency(s: &str) -> Result<f64, String> {
    let (num, mult) = strip_si_suffix(s, &[('k', 1e3), ('K', 1e3), ('m', 1e6), ('M', 1e6)]);
    let base: f64 = num.parse().map_err(|_| {
        format!("invalid frequency {s:?}; expected a number with an optional k/M suffix, e.g. f=1000 or f=1k")
    })?;
    Ok(base * mult)
}

fn parse_duration(s: &str) -> Result<f64, String> {
    const SUFFIXES: &[(&str, f64)] = &[
        ("ns", 1e-9),
        ("us", 1e-6),
        ("\u{b5}s", 1e-6), // µs
        ("ms", 1e-3),
        ("s", 1.0),
    ];
    for &(suffix, mult) in SUFFIXES {
        if let Some(num) = s.strip_suffix(suffix) {
            let base: f64 = num.parse().map_err(|_| {
                format!(
                    "invalid duration {s:?}; expected a number with a ns/us/ms/s suffix, e.g. >2ms"
                )
            })?;
            if base < 0.0 {
                return Err(format!("duration {s:?} must not be negative"));
            }
            return Ok(base * mult);
        }
    }
    Err(format!(
        "invalid duration {s:?}; expected a ns, us, µs, ms or s suffix, e.g. >2ms"
    ))
}

fn parse_voltage(s: &str) -> Result<f64, String> {
    let (num, _) = strip_si_suffix(s, &[('V', 1.0), ('v', 1.0)]);
    if num.len() == s.len() {
        return Err(format!(
            "invalid voltage {s:?}; expected a number ending in V, e.g. 1.2V"
        ));
    }
    num.parse()
        .map_err(|_| format!("invalid voltage {s:?}; expected a number ending in V, e.g. 1.2V"))
}

fn strip_si_suffix<'a>(s: &'a str, suffixes: &[(char, f64)]) -> (&'a str, f64) {
    if let Some(last) = s.chars().last() {
        for &(c, mult) in suffixes {
            if last == c {
                return (&s[..s.len() - last.len_utf8()], mult);
            }
        }
    }
    (s, 1.0)
}

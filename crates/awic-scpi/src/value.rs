//! Parameter values (SPECIFICATION.md §9's parameter grammar) and SCPI-style
//! number formatting/parsing.
//!
//! Deliberately pragmatic rather than exhaustively IEEE-488.2 compliant (the
//! crate's whole reason for existing, per the task brief): one flat [`Param`]
//! enum instead of a full arbitrary-block/expression grammar, and a numeric
//! suffix table tuned for what a script or AI assistant actually types
//! (`24MHZ` meaning 24 megahertz) rather than the historical SCPI convention
//! where bare `M` means *milli* and `MA` means mega. That historical choice
//! surprises virtually everyone who hasn't memorized it, so this parser
//! treats `M` as mega — a documented, deliberate deviation.

use crate::error::ScpiError;

/// One parsed parameter. Untyped on purpose: a header decides what shape it
/// expects (`as_number`, `as_bool`, ...) rather than the grammar trying to
/// know in advance, which is what lets one parser serve every command.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Number(f64),
    Str(String),
    Bool(bool),
    /// A bare word that isn't `ON`/`OFF`/a number/a string — `POS`, `NEG`,
    /// `AUTO`, `CH1`, `D3`, ... — uppercased at parse time (SCPI parameters
    /// are as case-insensitive as headers).
    Ident(String),
    /// `(@1,2,5)` — channel numbers as written, no implicit zero-basing.
    ChannelList(Vec<u32>),
}

impl Param {
    pub fn as_number(&self) -> Result<f64, ScpiError> {
        match self {
            Param::Number(v) => Ok(*v),
            _ => Err(ScpiError::IllegalParameterValue(format!(
                "expected a number, got {self:?}"
            ))),
        }
    }

    pub fn as_bool(&self) -> Result<bool, ScpiError> {
        match self {
            Param::Bool(v) => Ok(*v),
            Param::Number(v) if *v == 0.0 => Ok(false),
            Param::Number(v) if *v == 1.0 => Ok(true),
            Param::Ident(s) if s == "ON" => Ok(true),
            Param::Ident(s) if s == "OFF" => Ok(false),
            _ => Err(ScpiError::IllegalParameterValue(format!(
                "expected ON|OFF|1|0, got {self:?}"
            ))),
        }
    }

    pub fn as_str(&self) -> Result<&str, ScpiError> {
        match self {
            Param::Str(s) => Ok(s),
            _ => Err(ScpiError::IllegalParameterValue(format!(
                "expected a quoted string, got {self:?}"
            ))),
        }
    }

    pub fn as_ident(&self) -> Result<&str, ScpiError> {
        match self {
            Param::Ident(s) => Ok(s),
            _ => Err(ScpiError::IllegalParameterValue(format!(
                "expected a bare keyword, got {self:?}"
            ))),
        }
    }

    /// A `CH<n>` parameter (`MEAS:FREQ? CH1`), returned 0-based to match the
    /// server's internal channel numbering (`RecChannel`/`CaptureMeasure`
    /// name channels `CH<n+1>` 1-based for humans, SPEC §2.9.1).
    pub fn as_channel(&self) -> Result<u32, ScpiError> {
        let ident = self.as_ident()?;
        let digits = ident.strip_prefix("CH").ok_or_else(|| {
            ScpiError::IllegalParameterValue(format!("expected CHn, got {ident}"))
        })?;
        let n: u32 = digits
            .parse()
            .map_err(|_| ScpiError::IllegalParameterValue(format!("expected CHn, got {ident}")))?;
        n.checked_sub(1)
            .ok_or_else(|| ScpiError::DataOutOfRange("channel numbers start at CH1".into()))
    }

    pub fn as_channel_list(&self) -> Result<&[u32], ScpiError> {
        match self {
            Param::ChannelList(v) => Ok(v),
            _ => Err(ScpiError::IllegalParameterValue(format!(
                "expected a channel list (@..), got {self:?}"
            ))),
        }
    }
}

/// Parses one comma-separated parameter token — already split at top level by
/// [`crate::parser::split_top_level`] — into a typed [`Param`].
pub(crate) fn parse_param(token: &str) -> Result<Param, ScpiError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(ScpiError::MissingParameter);
    }
    if let Some(rest) = token.strip_prefix('"') {
        return parse_quoted_string(rest).map(Param::Str);
    }
    if let Some(rest) = token.strip_prefix('(') {
        return parse_channel_list(rest).map(Param::ChannelList);
    }
    let upper = token.to_ascii_uppercase();
    match upper.as_str() {
        "ON" => return Ok(Param::Bool(true)),
        "OFF" => return Ok(Param::Bool(false)),
        _ => {}
    }
    if let Some(n) = parse_number(token) {
        return Ok(Param::Number(n));
    }
    Ok(Param::Ident(upper))
}

/// `"..."` with the standard SCPI/IEEE-488.2 doubled-quote escape for a
/// literal `"` inside the string. `rest` is everything after the opening
/// quote.
fn parse_quoted_string(rest: &str) -> Result<String, ScpiError> {
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    loop {
        match chars.next() {
            Some('"') => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    out.push('"');
                } else {
                    return Ok(out);
                }
            }
            Some(c) => out.push(c),
            None => return Err(ScpiError::SyntaxError("unterminated string".into())),
        }
    }
}

/// `@1,2,5` (the part after `(@` up to `)`) — `rest` is everything after the
/// opening `(`.
fn parse_channel_list(rest: &str) -> Result<Vec<u32>, ScpiError> {
    let rest = rest.trim();
    let rest = rest
        .strip_prefix('@')
        .ok_or_else(|| ScpiError::SyntaxError("channel list must start with (@".into()))?;
    let rest = rest
        .strip_suffix(')')
        .ok_or_else(|| ScpiError::SyntaxError("unterminated channel list".into()))?;
    rest.split(',')
        .map(|part| {
            part.trim()
                .parse::<u32>()
                .map_err(|_| ScpiError::IllegalParameterValue(format!("bad channel {part:?}")))
        })
        .collect()
}

/// Numeric multipliers, longest-key-first so `"MA"` is tried before `"M"`.
/// **Deviation from strict SCPI** (module doc): `M` is mega here, not milli —
/// see the module doc for why.
const MULTIPLIERS: &[(&str, f64)] = &[
    ("EX", 1e18),
    ("PE", 1e15),
    ("TA", 1e12),
    ("MA", 1e6),
    ("KI", 1e3),
    ("GA", 1e9),
    ("G", 1e9),
    ("T", 1e12),
    ("K", 1e3),
    ("M", 1e6),
    ("U", 1e-6),
    ("N", 1e-9),
    ("P", 1e-12),
    ("F", 1e-15),
];

/// Parses a SCPI-flavoured number: an optional sign, digits, optional
/// fraction, optional `E`/`e` exponent, and an optional SI multiplier plus
/// unit suffix (`24E6`, `1.5`, `-2.5E-3`, `24MHZ`, `1K`, `100M`). Returns
/// `None` (not an error) so callers can fall back to treating the token as a
/// bare identifier instead — most SCPI keywords parse this way first.
pub fn parse_number(input: &str) -> Option<f64> {
    let bytes = input.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mantissa_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut saw_digit = i > mantissa_start;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        saw_digit |= i > frac_start;
    }
    if !saw_digit {
        return None;
    }
    let mut end = i;
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            end = j;
        }
    }
    let mantissa: f64 = input[..end].parse().ok()?;
    let suffix = &input[end..];
    if suffix.is_empty() {
        return Some(mantissa);
    }
    let suffix_upper = suffix.to_ascii_uppercase();
    for (key, factor) in MULTIPLIERS {
        if suffix_upper.starts_with(key) {
            return Some(mantissa * factor);
        }
    }
    // An unrecognized suffix is treated as a bare unit (`HZ`, `V`, `S`, ...)
    // with an implicit multiplier of 1 — pragmatic, since the unit itself
    // carries no information the engine needs (every command's unit is
    // fixed by its own semantics).
    Some(mantissa)
}

/// Quotes a string the SCPI way: wrapped in `"..."`, with any embedded `"`
/// doubled — the same escape [`parse_quoted_string`] undoes. Shared by
/// `SYST:ERR?`'s wire form and every command that answers with a string.
pub(crate) fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Formats a value the way the spec's own examples do: always scientific
/// notation, an explicit `+`/`-` on the exponent, and the shortest mantissa
/// that round-trips to 6 significant digits (`1.0002E+3`, `9.91E+37`).
pub fn format_scpi_number(v: f64) -> String {
    if v == 0.0 {
        return "0.0E+0".to_string();
    }
    if !v.is_finite() {
        return if v.is_sign_negative() {
            "-9.9E+37".to_string()
        } else {
            "9.9E+37".to_string()
        };
    }
    let neg = v.is_sign_negative();
    let abs = v.abs();
    let mut exp = abs.log10().floor() as i32;
    let mut mantissa = abs / 10f64.powi(exp);
    // log10/powi rounding can land just outside [1, 10); renormalize.
    if mantissa >= 10.0 {
        mantissa /= 10.0;
        exp += 1;
    } else if mantissa < 1.0 {
        mantissa *= 10.0;
        exp -= 1;
    }
    // 6 significant digits (1 before the point, 5 after), then round again
    // in case that rounding itself carried the mantissa to 10.0.
    mantissa = (mantissa * 1e5).round() / 1e5;
    if mantissa >= 10.0 {
        mantissa /= 10.0;
        exp += 1;
    }
    let mut mant_str = format!("{mantissa:.5}");
    while mant_str.ends_with('0') && !mant_str.ends_with(".0") {
        mant_str.pop();
    }
    let sign = if neg { "-" } else { "" };
    let exp_sign = if exp >= 0 { "+" } else { "-" };
    format!("{sign}{mant_str}E{exp_sign}{}", exp.abs())
}

/// Plain fixed-point seconds, 4 decimal places — the format SPECIFICATION.md
/// §9's own `REC:SEAR?`/`REC:WAIT?` examples use (`3.2140,5.8812`,
/// `41.0021`), distinct from [`format_scpi_number`]'s scientific notation
/// (`MEAS:FREQ?`'s `1.0002E+3`). Recorder match times are always
/// human-sized durations within one recording's retained span, so plain
/// decimal reads better than an exponent — the spec's own choice, not an
/// arbitrary one made here.
pub fn format_seconds(v: f64) -> String {
    format!("{v:.4}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_with_exponent_and_si_suffix() {
        assert_eq!(parse_number("24E6"), Some(24e6));
        assert_eq!(parse_number("1.5"), Some(1.5));
        assert_eq!(parse_number("-2.5E-3"), Some(-0.0025));
        assert_eq!(parse_number("24MHZ"), Some(24e6));
        assert_eq!(parse_number("1K"), Some(1000.0));
        assert_eq!(parse_number("1KHZ"), Some(1000.0));
        assert_eq!(parse_number("100M"), Some(100e6));
        assert_eq!(parse_number("1.5V"), Some(1.5));
        assert_eq!(parse_number("3"), Some(3.0));
        assert_eq!(parse_number("ON"), None);
        assert_eq!(parse_number(""), None);
    }

    #[test]
    fn formatting_matches_the_specs_own_examples() {
        assert_eq!(format_scpi_number(1000.2), "1.0002E+3");
        assert_eq!(format_scpi_number(9.91e37), "9.91E+37");
        assert_eq!(format_scpi_number(0.0), "0.0E+0");
        assert_eq!(format_scpi_number(-0.0025), "-2.5E-3");
        assert_eq!(format_scpi_number(24e6), "2.4E+7");
    }

    #[test]
    fn quoted_strings_support_the_doubled_quote_escape() {
        assert_eq!(
            parse_param(r#""hello""#).unwrap(),
            Param::Str("hello".into())
        );
        assert_eq!(
            parse_param(r#""say ""hi""""#).unwrap(),
            Param::Str(r#"say "hi""#.into())
        );
        assert!(matches!(
            parse_param(r#""unterminated"#),
            Err(ScpiError::SyntaxError(_))
        ));
    }

    #[test]
    fn booleans_accept_on_off_and_numeric_form() {
        assert!(parse_param("ON").unwrap().as_bool().unwrap());
        assert!(!parse_param("off").unwrap().as_bool().unwrap());
        assert!(parse_param("1").unwrap().as_bool().unwrap());
        assert!(!parse_param("0").unwrap().as_bool().unwrap());
    }

    #[test]
    fn channel_list_parses_at_positions() {
        let p = parse_param("(@1,2,5)").unwrap();
        assert_eq!(p.as_channel_list().unwrap(), &[1, 2, 5]);
    }

    #[test]
    fn seconds_use_plain_fixed_point_per_the_specs_recorder_examples() {
        assert_eq!(format_seconds(3.214), "3.2140");
        assert_eq!(format_seconds(5.8812), "5.8812");
        assert_eq!(format_seconds(41.0021), "41.0021");
    }

    #[test]
    fn ch_parameter_is_zero_based_for_the_server() {
        assert_eq!(parse_param("CH1").unwrap().as_channel().unwrap(), 0);
        assert_eq!(parse_param("CH2").unwrap().as_channel().unwrap(), 1);
        assert!(parse_param("CH0").unwrap().as_channel().is_err());
    }
}

//! Tokenizer for one SCPI program message (SPECIFICATION.md §9): splits a
//! line into its `;`-separated commands, applies the standard compound-
//! message path rule (a leading `:` resets to the root; otherwise a command
//! continues at the same tree level as the previous one), resolves each
//! header's mnemonics to their canonical long form (case-insensitive, short
//! or long form, the convention "headers are case-insensitive with long
//! and short forms"), and parses the trailing parameter list.
//!
//! One irregularity the grammar has to support: `USB:HUB "3-3":PORT3:POW`
//! embeds a quoted string *inside* the header chain rather than after it
//! (SPEC §9's own example). Rather than hard-coding which mnemonics allow
//! that, [`parse_header_chain`] uses a generic lookahead: after a mnemonic,
//! if a quoted string follows and a `:` follows *that*, it's a header
//! argument and the chain continues; otherwise it's the first data
//! parameter and header parsing stops. This handles any future mnemonic
//! with the same shape for free.

use crate::error::ScpiError;
use crate::value::{parse_param, Param};

/// One mnemonic in a header chain: its canonical long form, an optional
/// numeric suffix (`PORT3` -> `("PORT", Some(3))`), and an optional inline
/// string argument (`HUB "3-3"` -> `("HUB", None, Some("3-3"))`).
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub mnemonic: String,
    pub suffix: Option<u32>,
    pub inline_arg: Option<String>,
}

/// One parsed command from a compound message: either an IEEE-488.2 common
/// command (`*IDN?`, one bare segment with the leading `*` stripped) or a
/// hierarchical one (`segments` is the full resolved path).
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub is_common: bool,
    pub segments: Vec<Segment>,
    pub query: bool,
    pub params: Vec<Param>,
}

impl Command {
    /// The path joined with `:` for matching in `engine.rs` — e.g.
    /// `"USB:PORT:POWER"`. Cheaper to build once here than to slice-match
    /// `&[Segment]` at every call site.
    pub fn path(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.mnemonic.as_str())
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// Canonical (short, long) mnemonic pairs used anywhere in the command tree
/// (SPEC §9 plus the recorder commands). Matching is global rather than
/// per-tree-level: a word means the same thing wherever it appears, and the
/// full path (not one mnemonic in isolation) is what the engine matches on,
/// so there is no ambiguity in practice — deliberately simple for a bounded,
/// hand-known command set (task brief: "pragmatic ... not strictly SCPI
/// compliant").
const MNEMONICS: &[(&str, &str)] = &[
    ("INST", "INSTRUMENT"),
    ("CAT", "CATALOG"),
    ("SEL", "SELECT"),
    ("USB", "USB"),
    ("TREE", "TREE"),
    ("HUB", "HUB"),
    ("PORT", "PORT"),
    ("POW", "POWER"),
    ("LOCK", "LOCK"),
    ("ACQ", "ACQUIRE"),
    ("RATE", "RATE"),
    ("CHAN", "CHANNEL"),
    ("SAMP", "SAMPLES"),
    ("RANG", "RANGE"),
    ("TRIG", "TRIGGER"),
    ("EDGE", "EDGE"),
    ("SOUR", "SOURCE"),
    ("SLOP", "SLOPE"),
    ("LEV", "LEVEL"),
    ("STAR", "START"),
    ("STOP", "STOP"),
    ("MEAS", "MEASURE"),
    ("FREQ", "FREQUENCY"),
    ("PER", "PERIOD"),
    ("VPP", "VPP"),
    ("DUTY", "DUTY"),
    ("VMIN", "VMIN"),
    ("VMAX", "VMAX"),
    ("VAVG", "VAVG"),
    ("VRMS", "VRMS"),
    ("REC", "RECORD"),
    ("LIST", "LIST"),
    ("SEAR", "SEARCH"),
    ("WAIT", "WAIT"),
    ("SNAP", "SNAPSHOT"),
    ("SYST", "SYSTEM"),
    ("COMM", "COMMUNICATE"),
    ("INL", "INLINE"),
    ("ERR", "ERROR"),
    ("COUN", "COUNT"),
    ("DEC", "DECODE"),
    ("CAPT", "CAPTURE"),
    // SCPI-99's required status structure and SYSTem:VERSion?.
    ("STAT", "STATUS"),
    ("OPER", "OPERATION"),
    ("QUES", "QUESTIONABLE"),
    ("EVEN", "EVENT"),
    ("COND", "CONDITION"),
    ("ENAB", "ENABLE"),
    ("PRES", "PRESET"),
    ("VERS", "VERSION"),
];

/// Splits `raw`'s trailing ASCII digits off as a numeric suffix. Safe to
/// apply unconditionally: no mnemonic in [`MNEMONICS`] itself ends in a
/// digit, so any trailing digits found must be an index (`PORT3`).
fn split_trailing_digits(raw: &str) -> (&str, Option<u32>) {
    let split_at = raw
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if split_at == raw.len() {
        (raw, None)
    } else {
        (&raw[..split_at], raw[split_at..].parse().ok())
    }
}

fn canonicalize(raw: &str) -> Result<(String, Option<u32>), ScpiError> {
    let (alpha, suffix) = split_trailing_digits(raw);
    let upper = alpha.to_ascii_uppercase();
    for (short, long) in MNEMONICS {
        if upper == *short || upper == *long {
            return Ok(((*long).to_string(), suffix));
        }
    }
    Err(ScpiError::UndefinedHeader(raw.to_string()))
}

/// Splits `s` on top-level occurrences of `delim` — not inside a `"..."`
/// string (with its `""` escape) or `(...)` channel list. Used both to split
/// a program message on `;` and a parameter list on `,`.
fn split_top_level(s: &str, delim: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quotes = false;
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            current.push(c);
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    current.push(chars.next().unwrap());
                } else {
                    in_quotes = false;
                }
            }
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                current.push(c);
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            c if c == delim && depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// Parses a quoted string starting at `chars[start]` (which must be `"`).
/// Returns the unescaped content and the index just past the closing quote,
/// or `None` if the string never closes.
fn try_parse_quoted_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 1;
    let mut out = String::new();
    loop {
        let c = *chars.get(i)?;
        i += 1;
        if c == '"' {
            if chars.get(i) == Some(&'"') {
                out.push('"');
                i += 1;
            } else {
                return Some((out, i));
            }
        } else {
            out.push(c);
        }
    }
}

/// Parses one command's header chain. `input` has already had any leading
/// `:` stripped. Returns the resolved segments, whether the last mnemonic
/// carried a trailing `?`, and the raw (untrimmed-of-quotes) parameter text
/// that follows.
fn parse_header_chain(input: &str) -> Result<(Vec<Segment>, bool, String), ScpiError> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut segments = Vec::new();
    loop {
        let mnemonic_start = i;
        while i < chars.len() && chars[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i == mnemonic_start {
            return Err(ScpiError::SyntaxError(format!(
                "expected a header mnemonic in {input:?}"
            )));
        }
        let raw: String = chars[mnemonic_start..i].iter().collect();
        let (mnemonic, suffix) = canonicalize(&raw)?;

        if i < chars.len() && chars[i] == '?' {
            i += 1;
            segments.push(Segment {
                mnemonic,
                suffix,
                inline_arg: None,
            });
            let params: String = chars[i..].iter().collect();
            return Ok((segments, true, params.trim_start().to_string()));
        }

        // Lookahead for an inline header argument (`HUB "3-3":...`): a
        // quoted string immediately (optionally after one space) following
        // the mnemonic, itself immediately followed by `:`, continues the
        // header; anything else and this quoted string is the first data
        // parameter instead.
        let mut lookahead = i;
        if lookahead < chars.len() && chars[lookahead] == ' ' {
            lookahead += 1;
        }
        if lookahead < chars.len() && chars[lookahead] == '"' {
            if let Some((arg, after)) = try_parse_quoted_at(&chars, lookahead) {
                if after < chars.len() && chars[after] == ':' {
                    segments.push(Segment {
                        mnemonic,
                        suffix,
                        inline_arg: Some(arg),
                    });
                    i = after + 1;
                    continue;
                }
            }
        }

        segments.push(Segment {
            mnemonic,
            suffix,
            inline_arg: None,
        });
        if i < chars.len() && chars[i] == ':' {
            i += 1;
            continue;
        }
        let params: String = chars[i..].iter().collect();
        return Ok((segments, false, params.trim_start().to_string()));
    }
}

/// Parses a full program message: one or more `;`-separated commands.
pub fn parse_message(line: &str) -> Result<Vec<Command>, ScpiError> {
    let mut commands = Vec::new();
    // The parent path a non-rooted command continues from — the previous
    // command's path minus its own leaf mnemonic (the standard SCPI
    // compound-message rule; SPECIFICATION.md §9/task brief).
    let mut context_prefix: Vec<Segment> = Vec::new();

    for chunk in split_top_level(line, ';') {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('*') {
            // A common command may take parameters — `*ESE 32`, `*SRE 0` —
            // so the name ends at the first space or `?`, and the remainder
            // is a parameter list like any other command's. Discarding it,
            // as this parser first did, made `*ESE 32` an undefined header
            // and so made a generic VISA driver's startup sequence fail
            // (live test round 4, B-29).
            let (head, params_str) = match rest.find(|c: char| c.is_ascii_whitespace()) {
                Some(i) => (&rest[..i], rest[i..].trim()),
                None => (rest, ""),
            };
            let (name, query) = match head.strip_suffix('?') {
                Some(n) => (n.to_ascii_uppercase(), true),
                None => (head.to_ascii_uppercase(), false),
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(ScpiError::UndefinedHeader(trimmed.to_string()));
            }
            let params = if params_str.is_empty() {
                Vec::new()
            } else {
                split_top_level(params_str, ',')
                    .into_iter()
                    .map(|tok| parse_param(tok.trim()))
                    .collect::<Result<Vec<_>, _>>()?
            };
            commands.push(Command {
                is_common: true,
                segments: vec![Segment {
                    mnemonic: name,
                    suffix: None,
                    inline_arg: None,
                }],
                query,
                params,
            });
            // Common commands have no path of their own (IEEE-488.2): the
            // next unrooted command must specify a full path again.
            context_prefix.clear();
            continue;
        }

        let rooted = trimmed.starts_with(':');
        let body = if rooted { &trimmed[1..] } else { trimmed };
        let (mut segments, query, params_str) = parse_header_chain(body)?;
        if !rooted && !context_prefix.is_empty() {
            let mut full = context_prefix.clone();
            full.extend(segments);
            segments = full;
        }

        let params = if params_str.is_empty() {
            Vec::new()
        } else {
            split_top_level(&params_str, ',')
                .into_iter()
                .map(|tok| parse_param(tok.trim()))
                .collect::<Result<Vec<_>, _>>()?
        };

        context_prefix = if segments.len() > 1 {
            segments[..segments.len() - 1].to_vec()
        } else {
            Vec::new()
        };

        commands.push(Command {
            is_common: false,
            segments,
            query,
            params,
        });
    }

    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(line: &str) -> Vec<String> {
        parse_message(line)
            .unwrap()
            .iter()
            .map(|c| c.path())
            .collect()
    }

    #[test]
    fn long_and_short_forms_resolve_to_the_same_path() {
        assert_eq!(paths("INSTrument:CATalog?"), vec!["INSTRUMENT:CATALOG"]);
        assert_eq!(paths("INST:CAT?"), vec!["INSTRUMENT:CATALOG"]);
        assert_eq!(paths("instrument:catalog?"), vec!["INSTRUMENT:CATALOG"]);
    }

    #[test]
    fn a_common_command_keeps_its_parameters() {
        let commands = parse_message("*ESE 32").unwrap();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].is_common);
        assert_eq!(commands[0].segments[0].mnemonic, "ESE");
        assert!(!commands[0].query);
        assert_eq!(commands[0].params.len(), 1, "the 32 must survive parsing");
        assert_eq!(commands[0].params[0].as_number().unwrap(), 32.0);

        // A query still takes none, and a bare setter still takes none.
        assert!(parse_message("*ESE?").unwrap()[0].params.is_empty());
        assert!(parse_message("*CLS").unwrap()[0].params.is_empty());
        // Mixed into a compound message, next to a hierarchical command.
        let commands = parse_message("*CLS;*ESE 48;:ACQ:RATE 1000").unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[1].params[0].as_number().unwrap(), 48.0);
        assert_eq!(commands[2].path(), "ACQUIRE:RATE");
    }

    #[test]
    fn the_status_subsystem_resolves_in_both_forms() {
        assert_eq!(paths("STAT:OPER:ENAB?"), vec!["STATUS:OPERATION:ENABLE"]);
        assert_eq!(
            paths("status:questionable:condition?"),
            vec!["STATUS:QUESTIONABLE:CONDITION"]
        );
        assert_eq!(paths("STAT:PRES"), vec!["STATUS:PRESET"]);
        assert_eq!(paths("SYST:VERS?"), vec!["SYSTEM:VERSION"]);
    }

    #[test]
    fn numeric_suffix_is_captured_on_the_right_segment() {
        let commands = parse_message(r#"USB:HUB "3-3":PORT3:POW OFF"#).unwrap();
        assert_eq!(commands.len(), 1);
        let segs = &commands[0].segments;
        assert_eq!(segs[0].mnemonic, "USB");
        assert_eq!(segs[1].mnemonic, "HUB");
        assert_eq!(segs[1].inline_arg.as_deref(), Some("3-3"));
        assert_eq!(segs[2].mnemonic, "PORT");
        assert_eq!(segs[2].suffix, Some(3));
        assert_eq!(segs[3].mnemonic, "POWER");
        assert_eq!(commands[0].params, vec![Param::Bool(false)]);
    }

    #[test]
    fn conventional_port_power_form_has_no_inline_header_argument() {
        let commands = parse_message(r#"USB:PORT:POW "3-3",3,OFF"#).unwrap();
        assert_eq!(commands[0].path(), "USB:PORT:POWER");
        assert_eq!(
            commands[0].params,
            vec![
                Param::Str("3-3".into()),
                Param::Number(3.0),
                Param::Bool(false)
            ]
        );
    }

    #[test]
    fn a_query_after_the_leaf_mnemonic_is_recognized_even_with_inline_args() {
        let commands = parse_message(r#"USB:HUB "3-3":PORT3:POW?"#).unwrap();
        assert!(commands[0].query);
        assert!(commands[0].params.is_empty());
    }

    #[test]
    fn compound_message_without_leading_colon_continues_at_the_same_level() {
        // The standard rule: `LEV 1.5` after `SOUR CH1` is
        // `ACQ:TRIG:EDGE:LEV`, not a fresh root-level `LEV`.
        let commands = parse_message("ACQ:TRIG:EDGE:SOUR CH1;SLOP POS;LEV 1.5").unwrap();
        assert_eq!(
            commands.iter().map(|c| c.path()).collect::<Vec<_>>(),
            vec![
                "ACQUIRE:TRIGGER:EDGE:SOURCE",
                "ACQUIRE:TRIGGER:EDGE:SLOPE",
                "ACQUIRE:TRIGGER:EDGE:LEVEL",
            ]
        );
    }

    #[test]
    fn a_leading_colon_resets_to_the_root() {
        // Without the leading `:`, `ERR?` would continue from
        // `ACQUIRE:RATE`'s parent (the root, since `RATE` is a top-level
        // leaf) — this line instead checks the *explicit* reset still works
        // when the previous command left a non-trivial context.
        let commands = parse_message("ACQ:TRIG:EDGE:SOUR CH1;:SYST:ERR?").unwrap();
        assert_eq!(
            commands.iter().map(|c| c.path()).collect::<Vec<_>>(),
            vec!["ACQUIRE:TRIGGER:EDGE:SOURCE", "SYSTEM:ERROR"]
        );

        let commands = parse_message("ACQ:RATE 1E6;:SYST:ERR?").unwrap();
        assert_eq!(
            commands.iter().map(|c| c.path()).collect::<Vec<_>>(),
            vec!["ACQUIRE:RATE", "SYSTEM:ERROR"]
        );
    }

    #[test]
    fn common_commands_reset_the_compound_context() {
        // After `*CLS`, `LEV 1.5` has no path memory to continue from — it
        // resolves to the bare mnemonic `LEVEL`, not
        // `ACQUIRE:TRIGGER:EDGE:LEVEL` (which the engine would reject as an
        // undefined header, but that's an engine-level check, not a parse
        // error: the parser's job here is only to *not* silently keep the
        // old context across a common command).
        let commands = parse_message("ACQ:TRIG:EDGE:SOUR CH1;*CLS;LEV 1.5").unwrap();
        assert_eq!(commands[2].path(), "LEVEL");
    }

    #[test]
    fn common_command_query_and_bare_forms() {
        let commands = parse_message("*IDN?;*RST;*OPC?").unwrap();
        assert!(commands[0].is_common && commands[0].query);
        assert_eq!(commands[0].segments[0].mnemonic, "IDN");
        assert!(commands[1].is_common && !commands[1].query);
        assert_eq!(commands[1].segments[0].mnemonic, "RST");
    }

    #[test]
    fn unknown_header_is_a_named_error() {
        let err = parse_message("FOO:BAR?").unwrap_err();
        assert_eq!(err.code(), -113);
    }

    #[test]
    fn channel_list_parameter_round_trips() {
        let commands = parse_message("ACQ:CHAN (@1,2)").unwrap();
        assert_eq!(commands[0].params[0].as_channel_list().unwrap(), &[1, 2]);
    }

    #[test]
    fn semicolon_inside_a_quoted_string_does_not_split_the_message() {
        let commands = parse_message(r#"REC:SEAR? "a;b",5"#).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].params[0].as_str().unwrap(), "a;b");
    }
}

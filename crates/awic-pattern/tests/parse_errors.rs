//! Table tests: one per parse error class, with the exact byte span
//! (SPECIFICATION.md §2.9.1: "parse errors point at the offending token").

mod support;

use awic_pattern::{parse, Span};
use support::{channels, rec_1mhz};

fn expect_span(text: &str, expected: Span) {
    let err = parse(text, &channels(), rec_1mhz())
        .expect_err(&format!("expected {text:?} to fail to parse"));
    assert_eq!(
        err.span,
        expected,
        "text={text:?} message={:?} token={:?}",
        err.message,
        &text[err.span.start.min(text.len())..err.span.end.min(text.len())]
    );
}

#[test]
fn empty_pattern() {
    expect_span("", Span::at(0));
}

#[test]
fn unknown_channel_lists_available() {
    let err = parse("D9 rising", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(0, 2));
    assert!(err.message.contains("D9"));
    assert!(err.message.contains("D0"));
    assert!(err.message.contains("CH1"));
}

#[test]
fn unknown_channel_in_clk() {
    let err = parse("D1 clk=D9 rising 10", &channels(), rec_1mhz()).unwrap_err();
    // "clk=D9" starts at byte 3; "D9" is at offset 7..9.
    assert_eq!(err.span, Span::new(7, 9));
}

#[test]
fn bad_frequency_value() {
    // "D0 f=abc 1010": f_val "abc" spans bytes 5..8.
    let err = parse("D0 f=abc 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(5, 8));
    assert!(err.message.contains("abc"));
}

#[test]
fn frequency_above_nyquist() {
    // rate=1MHz, nyquist=500kHz; f=600k is refused.
    let err = parse("D0 f=600k 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(5, 9));
    assert!(err.message.contains("500000") || err.message.contains("500,000"));
}

#[test]
fn frequency_zero_rejected() {
    let err = parse("D0 f=0 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(5, 6));
}

#[test]
fn invalid_bit_character() {
    // "D0 f=1k 102": the '2' is at byte 10.
    let err = parse("D0 f=1k 102", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(10, 11));
    assert!(err.message.contains('2'));
}

#[test]
fn tol_out_of_range() {
    // "D0 f=1k tol=0.9 1010": the value "0.9" spans bytes 12..15.
    let err = parse("D0 f=1k tol=0.9 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(12, 15));
}

#[test]
fn state_length_mismatch() {
    // "D3..D0 = 101": 4 channels but 3 bits, at bytes 9..12.
    let err = parse("D3..D0 = 101", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(9, 12));
    assert!(err.message.contains('4'));
    assert!(err.message.contains('3'));
}

#[test]
fn state_reversed_range() {
    let err = parse("D0..D3 = 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(0, 6));
}

#[test]
fn state_all_x_matches_nothing() {
    let err = parse("D3..D0 = xxxx", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(9, 13));
}

#[test]
fn pulse_two_lower_bounds() {
    let err = parse("D1 high >2ms >5ms", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(13, 17));
}

#[test]
fn pulse_missing_bound() {
    let err = parse("D1 high", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(3, 7));
}

#[test]
fn clocked_same_channel() {
    let err = parse("D1 clk=D1 rising 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(7, 9));
}

#[test]
fn clocked_rejects_bare_edge() {
    let err = parse("D1 clk=D0 edge 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(10, 14));
}

#[test]
fn analog_clause_on_digital_channel() {
    let err = parse("D0 >1.5V", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(0, 2));
    assert!(err.message.contains("digital"));
}

#[test]
fn digital_clause_on_analog_channel() {
    let err = parse("CH1 f=1000 1010", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(0, 3));
    assert!(err.message.contains("analog"));
}

#[test]
fn edge_rejects_trailing_tokens() {
    let err = parse("D0 rising 1.2V", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(10, 14));
}

#[test]
fn unrecognized_clause() {
    let err = parse("D0 zzz", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(3, 6));
}

#[test]
fn missing_clause_after_channel() {
    let err = parse("D0", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::at(2));
}

#[test]
fn bad_voltage() {
    // "CH1 rising abc": "abc" spans bytes 11..14.
    let err = parse("CH1 rising abc", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::new(11, 14));
}

#[test]
fn analog_edge_missing_voltage() {
    let err = parse("CH1 rising", &channels(), rec_1mhz()).unwrap_err();
    assert_eq!(err.span, Span::at(10));
}

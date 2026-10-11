//! Table tests: one per syntax form in SPECIFICATION.md §2.9.1.

mod support;

use awic_pattern::{parse, BitVal, Cmp, EdgeDir, FireOn, Pattern};
use support::{channels, rec_1mhz};

#[test]
fn bit_sequence_basic() {
    let p = parse("D0 f=1000 100010x101", &channels(), rec_1mhz()).unwrap();
    let Pattern::BitSeq(p) = p else {
        panic!("expected BitSeq, got {p:?}")
    };
    assert_eq!(p.bit, 0);
    assert_eq!(p.cell_samples, 1000.0); // 1e6 Hz / 1000 bit/s
    assert_eq!(p.tol_cells, 0.3);
    assert_eq!(
        p.bits,
        vec![
            BitVal::One,
            BitVal::Zero,
            BitVal::Zero,
            BitVal::Zero,
            BitVal::One,
            BitVal::Zero,
            BitVal::X,
            BitVal::One,
            BitVal::Zero,
            BitVal::One,
        ]
    );
}

#[test]
fn bit_sequence_si_suffix_and_comma_and_case() {
    let p = parse("d0 F=1k, 1010", &channels(), rec_1mhz()).unwrap();
    let Pattern::BitSeq(p) = p else {
        panic!("expected BitSeq, got {p:?}")
    };
    assert_eq!(p.cell_samples, 1000.0);
}

#[test]
fn bit_sequence_mega_suffix() {
    let rec = awic_pattern::RecordingInfo {
        rate_hz: 100_000_000.0,
        sample_bits: 8,
        channel_count: 0,
    };
    let p = parse("D0 f=1M 10", &channels(), rec).unwrap();
    let Pattern::BitSeq(p) = p else {
        panic!("expected BitSeq, got {p:?}")
    };
    assert_eq!(p.cell_samples, 100.0);
}

#[test]
fn bit_sequence_custom_tol() {
    let p = parse("D0 f=1k tol=0.2 1010", &channels(), rec_1mhz()).unwrap();
    let Pattern::BitSeq(p) = p else {
        panic!("expected BitSeq, got {p:?}")
    };
    assert_eq!(p.tol_cells, 0.2);
}

#[test]
fn edge_rising() {
    let p = parse("D2 rising", &channels(), rec_1mhz()).unwrap();
    let Pattern::Edge(p) = p else {
        panic!("expected Edge, got {p:?}")
    };
    assert_eq!(p.bit, 2);
    assert_eq!(p.dir, EdgeDir::Rising);
}

#[test]
fn edge_falling() {
    let p = parse("D2 falling", &channels(), rec_1mhz()).unwrap();
    let Pattern::Edge(p) = p else {
        panic!("expected Edge, got {p:?}")
    };
    assert_eq!(p.dir, EdgeDir::Falling);
}

#[test]
fn edge_either() {
    let p = parse("D2 edge", &channels(), rec_1mhz()).unwrap();
    let Pattern::Edge(p) = p else {
        panic!("expected Edge, got {p:?}")
    };
    assert_eq!(p.dir, EdgeDir::Either);
}

#[test]
fn pulse_both_bounds() {
    let p = parse("D1 high >2ms <5ms", &channels(), rec_1mhz()).unwrap();
    let Pattern::Pulse(p) = p else {
        panic!("expected Pulse, got {p:?}")
    };
    assert_eq!(p.bit, 1);
    assert!(p.high);
    assert_eq!(p.min, Some((Cmp::Gt, 2000.0)));
    assert_eq!(p.max, Some((Cmp::Lt, 5000.0)));
}

#[test]
fn pulse_held_lower_only() {
    let p = parse("D1 low >=10ms", &channels(), rec_1mhz()).unwrap();
    let Pattern::Pulse(p) = p else {
        panic!("expected Pulse, got {p:?}")
    };
    assert!(!p.high);
    assert_eq!(p.min, Some((Cmp::Ge, 10_000.0)));
    assert_eq!(p.max, None);
}

#[test]
fn state_range_msb_first() {
    let p = parse("D3..D0 = 1x01", &channels(), rec_1mhz()).unwrap();
    let Pattern::State(p) = p else {
        panic!("expected State, got {p:?}")
    };
    // D3=1 D2=x D1=0 D0=1
    assert_eq!(p.mask, 0b1011);
    assert_eq!(p.expected, 0b1001);
}

#[test]
fn state_comma_list() {
    let p = parse("D0,D2 = 10", &channels(), rec_1mhz()).unwrap();
    let Pattern::State(p) = p else {
        panic!("expected State, got {p:?}")
    };
    // list order D0,D2 maps MSB-first onto "10": D0=1, D2=0
    assert_eq!(p.mask, 0b0101);
    assert_eq!(p.expected, 0b0001);
}

#[test]
fn clocked_rising() {
    let p = parse("D1 clk=D0 rising 1010x", &channels(), rec_1mhz()).unwrap();
    let Pattern::Clocked(p) = p else {
        panic!("expected Clocked, got {p:?}")
    };
    assert_eq!(p.data_bit, 1);
    assert_eq!(p.clk_bit, 0);
    assert_eq!(p.clk_dir, EdgeDir::Rising);
    assert_eq!(p.bits.len(), 5);
}

#[test]
fn clocked_falling() {
    let p = parse("D1 clk=D0 falling 10", &channels(), rec_1mhz()).unwrap();
    let Pattern::Clocked(p) = p else {
        panic!("expected Clocked, got {p:?}")
    };
    assert_eq!(p.clk_dir, EdgeDir::Falling);
}

#[test]
fn analog_edge_rising() {
    let p = parse("CH1 rising 1.2V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert_eq!(p.fire_on, FireOn::Rising);
    // scale=0.01, offset=-1.28 => code = (1.2 - (-1.28))/0.01 = 248
    // (loose tolerance: offset/scale are f32, like real channel calibration)
    assert!((p.threshold_code - 248.0).abs() < 1e-3);
}

#[test]
fn analog_edge_falling() {
    let p = parse("CH1 falling 1.2V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert_eq!(p.fire_on, FireOn::Falling);
}

#[test]
fn analog_cross() {
    let p = parse("CH1 cross 1.2V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert_eq!(p.fire_on, FireOn::Both);
}

#[test]
fn analog_level_gt() {
    let p = parse("CH1 >1.5V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert_eq!(p.fire_on, FireOn::Rising);
}

#[test]
fn analog_level_lt() {
    let p = parse("CH1 <1.5V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert_eq!(p.fire_on, FireOn::Falling);
}

#[test]
fn analog_level_with_hyst() {
    let p = parse("CH1 >=1.5V hyst=0.05V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    // hyst volts / |scale| = 0.05/0.01 = 5 codes
    assert!((p.hyst_code - 5.0).abs() < 1e-6);
}

#[test]
fn analog_default_hyst_is_a_few_codes() {
    let p = parse("CH1 >1.5V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert_eq!(p.hyst_code, 3.0);
}

#[test]
fn negative_scale_hysteresis_stays_positive_width() {
    // CH2 has scale = -0.01: an inverted channel. hyst must still widen the
    // band, never flip its sign.
    let p = parse("CH2 >1.5V hyst=0.05V", &channels(), rec_1mhz()).unwrap();
    let Pattern::Analog(p) = p else {
        panic!("expected Analog, got {p:?}")
    };
    assert!(p.hyst_code > 0.0);
}

/// The user's first example had no channel: with one digital channel there
/// is nothing to disambiguate.
#[test]
fn bit_sequence_without_channel_uses_the_only_digital_channel() {
    let only_d3 = vec![awic_pattern::ChannelInfo {
        name: "D3".into(),
        kind: awic_pattern::ChannelKind::Digital { bit: 3 },
    }];
    let p = parse("f=1000, 100010x101", &only_d3, rec_1mhz()).unwrap();
    let Pattern::BitSeq(p) = p else {
        panic!("expected BitSeq, got {p:?}")
    };
    assert_eq!(p.bit, 3);
    assert_eq!(p.bits.len(), 10);
}

#[test]
fn bit_sequence_without_channel_is_refused_when_ambiguous() {
    let err = parse("f=1000, 100010x101", &channels(), rec_1mhz()).unwrap_err();
    assert!(err.message.contains("name the channel first"), "{err:?}");
    assert!(err.message.contains("D0, D1"), "{err:?}");
}

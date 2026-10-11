//! Semantic tests for analog crossings with hysteresis (SPEC §2.9.1).

mod support;

use awic_pattern::{parse, Matcher};
use support::{channels, rec_1mhz_1ch};

#[test]
fn rising_crossing_with_clean_signal() {
    // CH1: scale=0.01 offset=-1.28 => code = (v+1.28)/0.01. Threshold 0V =>
    // code 128.
    let pattern = parse("CH1 rising 0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let codes: Vec<u8> = vec![50, 60, 70, 200, 210, 220];
    let matches = matcher.feed_analog(0, &codes);
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].sample, 3); // first sample >= enter_high
}

#[test]
fn falling_crossing() {
    let pattern = parse("CH1 falling 0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let codes: Vec<u8> = vec![200, 210, 220, 50, 40, 30];
    let matches = matcher.feed_analog(0, &codes);
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0].sample, 3);
}

#[test]
fn cross_fires_on_both_directions() {
    let pattern = parse("CH1 cross 0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let codes: Vec<u8> = vec![50, 60, 200, 210, 50, 40];
    let matches = matcher.feed_analog(0, &codes);
    assert_eq!(matches.len(), 2, "{matches:?}");
}

#[test]
fn level_gt_is_same_event_as_rising() {
    let pattern = parse("CH1 >0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let codes: Vec<u8> = vec![50, 60, 200, 210];
    let matches = matcher.feed_analog(0, &codes);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].sample, 2);
}

#[test]
fn level_lt_is_same_event_as_falling() {
    let pattern = parse("CH1 <0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let codes: Vec<u8> = vec![200, 210, 50, 40];
    let matches = matcher.feed_analog(0, &codes);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].sample, 2);
}

#[test]
fn noise_at_threshold_does_not_burst_without_hysteresis_crossing() {
    // Default hysteresis is 3 codes; threshold code = 128 (0V), so the
    // band is roughly [125,131]. Noise oscillating strictly inside that
    // band must not produce any crossings.
    let pattern = parse("CH1 cross 0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let codes: Vec<u8> = vec![126, 128, 127, 129, 126, 130, 127];
    let matches = matcher.feed_analog(0, &codes);
    assert!(matches.is_empty(), "{matches:?}");
}

#[test]
fn noise_riding_a_real_transition_produces_exactly_one_crossing_each_way() {
    // Simulates a step from low to high with a few noisy samples near the
    // threshold before settling, then back down. Hysteresis should absorb
    // the wobble and report exactly one rising and one falling crossing.
    let pattern = parse("CH1 cross 0V hyst=0.05V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    // hyst=0.05V / 0.01 = 5 codes => band [123,133] around 128.
    let codes: Vec<u8> = vec![
        50, 60, 100, 120, 129, 124, 135, 200, 210, // settles high
        200, 190, 135, 129, 124, 133, 60, 50, // settles low
    ];
    let matches = matcher.feed_analog(0, &codes);
    assert_eq!(matches.len(), 2, "{matches:?}");
}

#[test]
fn try_skip_analog_agrees_with_feeding_samples() {
    // Build a signal with a long flat stretch above the band, feed it two
    // ways — sample by sample, and via try_skip_analog for the flat span —
    // and confirm identical results (SPEC §2.8.3 pruning requirement).
    let pattern = parse("CH1 rising 0V", &channels(), rec_1mhz_1ch()).unwrap();

    let mut codes: Vec<u8> = vec![50, 60, 70, 200];
    codes.extend(std::iter::repeat_n(200, 1000)); // long flat high stretch
    codes.push(50); // drop back down (not part of this pattern's interest)

    let mut direct = Matcher::new(&pattern);
    let direct_matches = direct.feed_analog(0, &codes);

    let mut skipping = Matcher::new(&pattern);
    let head = &codes[0..4];
    let mut m = skipping.feed_analog(0, head);
    // Skip the flat stretch: min==max==200, safely above the band either
    // way `is_high` could be at this point.
    let skipped = skipping.try_skip_analog(4, 1000, 200, 200);
    assert!(skipped);
    m.extend(skipping.feed_analog(1004, &codes[1004..]));

    assert_eq!(direct_matches, m);
}

#[test]
fn try_skip_analog_refuses_when_span_straddles_the_band() {
    let pattern = parse("CH1 rising 0V", &channels(), rec_1mhz_1ch()).unwrap();
    let mut matcher = Matcher::new(&pattern);
    let _ = matcher.feed_analog(0, &[50]); // establish is_high = false
                                           // A bucket whose min/max straddle the hysteresis band must not be
                                           // skipped: the caller has to look at the actual samples.
    let skipped = matcher.try_skip_analog(1, 10, 50, 200);
    assert!(!skipped);
}

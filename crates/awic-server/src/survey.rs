//! `survey` (live test F-04): wraps `awic_decode::survey` for a finished
//! capture, and converts its result to the wire form — seconds instead of
//! samples, `D<n>` names, and a one-line summary per channel that already
//! folds in what the protocol guesses say about it. The recording variant
//! lives in `recorder::survey`, which streams runs through the same
//! `Surveyor`.

use awic_decode::survey::{Activity, Guess, Survey, Surveyor};
use awic_proto::{ChannelSurveyInfo, ProtocolGuess, SurveyActivity, SurveyResult};

use crate::decode::DecodeOpError;

/// Surveys `[t0_s, t1_s)` of a digital capture (default: all of it).
pub fn survey_capture(
    capture: &awic_core::Capture,
    capture_id: String,
    t0_s: Option<f64>,
    t1_s: Option<f64>,
) -> Result<SurveyResult, DecodeOpError> {
    let awic_core::CaptureData::Digital {
        transitions,
        packed,
    } = &capture.data
    else {
        return Err(DecodeOpError::new(
            "not_digital",
            "survey needs a digital capture (for an analog one, `capture` already reports \
             Vpp/frequency/duty per channel)",
        ));
    };
    let rate = capture.meta.rate_hz.max(1);
    let len = packed.len() as u64;
    let start = t0_s.map_or(0, |s| ((s * rate as f64).round().max(0.0) as u64).min(len));
    let end = t1_s.map_or(len, |s| {
        ((s * rate as f64).round().max(0.0) as u64).min(len)
    });
    if start >= end {
        return Err(DecodeOpError::new(
            "empty_range",
            format!("survey range [{start}, {end}) samples is empty; the capture holds {len}"),
        ));
    }
    let mut surveyor = Surveyor::new(&capture.meta.channels, rate, start, packed[start as usize]);
    let input: Vec<awic_decode::Transition> = transitions
        .iter()
        .filter(|t| t.sample_index > start && t.sample_index < end)
        .map(|t| awic_decode::Transition {
            sample_index: t.sample_index,
            value: t.channel_mask,
        })
        .collect();
    surveyor.push(&input);
    let mut result = to_wire(surveyor.finish(end), rate);
    result.capture = Some(capture_id);
    Ok(result)
}

/// Converts a survey to the wire form (`capture`/`rec` left for the caller).
pub fn to_wire(survey: Survey, rate_hz: u64) -> SurveyResult {
    let rate = rate_hz.max(1) as f64;
    let secs = |samples: u64| samples as f64 / rate;
    let channels = survey
        .channels
        .iter()
        .map(|c| ChannelSurveyInfo {
            channel: c.bit,
            name: format!("D{}", c.bit),
            summary: summary(c, &survey.guesses, rate),
            activity: match c.activity {
                Activity::Constant => SurveyActivity::Constant,
                Activity::GlitchesOnly => SurveyActivity::GlitchesOnly,
                Activity::Undersampled => SurveyActivity::Undersampled,
                Activity::Bursty => SurveyActivity::Bursty,
                Activity::Continuous => SurveyActivity::Continuous,
            },
            idle_high: c.idle_level,
            edges: c.edges,
            glitches: c.glitches,
            high_fraction: round(c.high_fraction, 4),
            min_high_s: c.min_high_samples.map(secs),
            min_low_s: c.min_low_samples.map(secs),
            median_pulse_s: c.median_pulse_samples.map(secs),
            first_edge_s: c.first_edge.map(secs),
            last_edge_s: c.last_edge.map(secs),
            bursts: if c.activity == Activity::Constant {
                0
            } else {
                c.bursts
            },
            burst_period_s: c.burst_period_samples.map(|p| p / rate),
        })
        .collect();
    let guesses = survey
        .guesses
        .iter()
        .map(|g| match g {
            Guess::Uart {
                channel,
                estimate,
                inverted,
            } => ProtocolGuess::Uart {
                channel: *channel,
                baud: estimate.baud,
                measured_baud: round(estimate.measured, 1),
                fit: round(estimate.fit, 3),
                inverted: *inverted,
            },
            Guess::I2c {
                scl,
                sda,
                starts,
                clock_hz,
            } => ProtocolGuess::I2c {
                scl: *scl,
                sda: *sda,
                starts: *starts,
                clock_hz: clock_hz.map(|f| round(f, 0)),
            },
            Guess::Clock {
                channel,
                frequency_hz,
                duty,
            } => ProtocolGuess::Clock {
                channel: *channel,
                frequency_hz: round(*frequency_hz, 3),
                duty: round(*duty, 3),
            },
            Guess::Spi {
                clk,
                data,
                cs,
                cs_active_high,
                mode,
                clock_hz,
                undersampled,
            } => ProtocolGuess::Spi {
                clk: *clk,
                data: data.clone(),
                cs: *cs,
                cs_active_high: *cs_active_high,
                mode: *mode,
                clock_hz: clock_hz.map(|f| round(f, 0)),
                undersampled: *undersampled,
            },
        })
        .collect();
    SurveyResult {
        capture: None,
        rec: None,
        rate_hz,
        t0_s: secs(survey.start_sample),
        t1_s: secs(survey.end_sample),
        channels,
        guesses,
        note: None,
    }
}

fn round(v: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (v * f).round() / f
}

fn level(high: bool) -> &'static str {
    if high {
        "high"
    } else {
        "low"
    }
}

/// `1.5 kHz`, `364 kHz`, `2 Hz`.
fn si_hz(hz: f64) -> String {
    if hz >= 1e6 {
        format!("{:.3} MHz", hz / 1e6)
    } else if hz >= 1e3 {
        format!("{:.3} kHz", hz / 1e3)
    } else {
        format!("{hz:.3} Hz")
    }
}

/// `16 MS/s`, `0.5 MS/s`.
fn si_rate(rate: f64) -> String {
    format!("{} MS/s", round(rate / 1e6, 3))
}

/// `500 ms`, `2.75 µs`.
fn si_s(s: f64) -> String {
    if s >= 1.0 {
        format!("{s:.3} s")
    } else if s >= 1e-3 {
        format!("{:.3} ms", s * 1e3)
    } else {
        format!("{:.3} µs", s * 1e6)
    }
}

fn summary(c: &awic_decode::survey::ChannelSurvey, guesses: &[Guess], rate: f64) -> String {
    for g in guesses {
        match g {
            Guess::I2c {
                scl,
                sda,
                starts,
                clock_hz,
            } if *scl == c.bit => {
                let clock = clock_hz
                    .map(|f| format!(" at ~{}", si_hz(f)))
                    .unwrap_or_default();
                return format!("I2C SCL (data on D{sda}){clock}, {starts} transactions");
            }
            Guess::I2c { scl, sda, .. } if *sda == c.bit => {
                return format!("I2C SDA (clock on D{scl})");
            }
            Guess::Uart {
                channel,
                estimate,
                inverted,
            } if *channel == c.bit => {
                let polarity = if *inverted {
                    "idles low: decode with invert"
                } else {
                    "idles high"
                };
                return format!("UART at {} baud ({polarity})", estimate.baud);
            }
            Guess::Clock {
                channel,
                frequency_hz,
                duty,
            } if *channel == c.bit => {
                return format!(
                    "clock {} at {:.0} % duty",
                    si_hz(*frequency_hz),
                    duty * 100.0
                );
            }
            Guess::Spi {
                clk,
                data,
                cs,
                cs_active_high,
                mode,
                clock_hz,
                undersampled,
            } => {
                let names = |bits: &[u8]| {
                    bits.iter()
                        .map(|b| format!("D{b}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                if *clk == c.bit {
                    let clock = if *undersampled {
                        format!(
                            " too fast to time at {} (pulses of 1-2 samples): capture faster \
                             if the device can",
                            si_rate(rate)
                        )
                    } else {
                        clock_hz
                            .map(|f| format!(" at ~{}", si_hz(f)))
                            .unwrap_or_default()
                    };
                    let cs = cs.map(|b| format!(", CS on D{b}")).unwrap_or_default();
                    return format!(
                        "SPI clock{clock}, mode {mode} (data on {}{cs})",
                        names(data)
                    );
                }
                if data.contains(&c.bit) {
                    return format!("SPI data, MOSI or MISO (clock on D{clk}, mode {mode})");
                }
                if *cs == Some(c.bit) {
                    return if *cs_active_high {
                        format!(
                            "SPI chip select, active high (clock on D{clk}): decode with \
                             cs_active_high"
                        )
                    } else {
                        format!("SPI chip select, active low (clock on D{clk})")
                    };
                }
            }
            _ => {}
        }
    }
    match c.activity {
        Activity::Constant => format!("constant {}", level(c.idle_level)),
        Activity::GlitchesOnly => format!(
            "rests {}; {} spikes of ≤2 samples — floating input or crosstalk",
            level(c.idle_level),
            c.glitches
        ),
        Activity::Undersampled => format!(
            "toggles at the sampling limit ({} edges in pulses of 1-2 samples) — a signal too \
             fast for {}: capture faster",
            c.edges,
            si_rate(rate)
        ),
        Activity::Bursty | Activity::Continuous => {
            let mut s = format!("active, rests {}, {} edges", level(c.idle_level), c.edges);
            if let Some(p) = c.burst_period_samples {
                s.push_str(&format!(", bursts every {}", si_s(p / rate)));
            } else if c.bursts > 1 {
                s.push_str(&format!(", {} bursts", c.bursts));
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awic_decode::Transition;

    /// SPI mode 0 at 1 MS/s: CLK on D0, MOSI on D1, CS (active low) on D2,
    /// eight 2-byte transfers 2 ms apart, 100 kHz clock.
    fn spi_mode0() -> (Vec<Transition>, u64) {
        let mut events: Vec<(u64, u8, bool)> = Vec::new();
        for n in 0..8u64 {
            let mut t = 100 + n * 2_000;
            events.push((t, 2, false));
            t += 5;
            for byte in [0x9Fu8, 0x5A ^ n as u8] {
                for i in (0..8).rev() {
                    events.push((t - 2, 1, (byte >> i) & 1 == 1));
                    events.push((t, 0, true));
                    events.push((t + 5, 0, false));
                    t += 10;
                }
            }
            events.push((t, 2, true));
        }
        events.sort_by_key(|e| e.0);
        let mut value = 0b100u8;
        let mut out = vec![Transition {
            sample_index: 0,
            value,
        }];
        for (at, bit, high) in events {
            let next = if high {
                value | 1 << bit
            } else {
                value & !(1 << bit)
            };
            if next != value {
                value = next;
                out.push(Transition {
                    sample_index: at,
                    value,
                });
            }
        }
        (out, 20_000)
    }

    #[test]
    fn an_spi_bus_is_named_with_its_roles_and_mode() {
        let (t, end) = spi_mode0();
        let mut s = Surveyor::new(&[0, 1, 2, 3], 1_000_000, 0, t[0].value);
        s.push(&t[1..]);
        let result = to_wire(s.finish(end), 1_000_000);
        assert_eq!(
            result.guesses,
            vec![ProtocolGuess::Spi {
                clk: 0,
                data: vec![1],
                cs: Some(2),
                cs_active_high: false,
                mode: 0,
                clock_hz: Some(100_000.0),
                undersampled: false,
            }]
        );
        let summaries: Vec<&str> = result.channels.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "SPI clock at ~100.000 kHz, mode 0 (data on D1, CS on D2)",
                "SPI data, MOSI or MISO (clock on D0, mode 0)",
                "SPI chip select, active low (clock on D0)",
                "constant low",
            ]
        );
        let json = serde_json::to_value(&result.guesses[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"protocol":"spi","clk":0,"data":[1],"cs":2,"mode":0,"clock_hz":100000.0})
        );
    }
}

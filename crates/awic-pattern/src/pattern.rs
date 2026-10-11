//! The compiled pattern: the result of [`crate::parse`]. Every unit the
//! syntax accepts (Hz, seconds, volts) has already been converted to
//! samples or raw codes here, using the recording's rate and calibration —
//! the matchers never see a physical unit.

/// One bit cell's expected value in a bit-sequence or clocked pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitVal {
    Zero,
    One,
    /// `x`: matches either level.
    X,
}

impl BitVal {
    pub fn matches(self, bit_is_set: bool) -> bool {
        match self {
            BitVal::Zero => !bit_is_set,
            BitVal::One => bit_is_set,
            BitVal::X => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
}

impl Cmp {
    /// True if `len` (a run/pulse duration in samples) satisfies `self`
    /// against `threshold` (samples, possibly fractional). A small epsilon
    /// absorbs the float error of converting a decimal duration to samples.
    pub fn holds(self, len: u64, threshold: f64) -> bool {
        const EPS: f64 = 1e-6;
        let l = len as f64;
        match self {
            Cmp::Gt => l > threshold + EPS,
            Cmp::Ge => l >= threshold - EPS,
            Cmp::Lt => l < threshold - EPS,
            Cmp::Le => l <= threshold + EPS,
        }
    }

    /// True if this is a lower bound (`>`, `>=`).
    pub fn is_lower(self) -> bool {
        matches!(self, Cmp::Gt | Cmp::Ge)
    }

    /// The smallest sample length `len` for which `holds(len, threshold)` is
    /// true, for a lower-bound comparison. Used to report a level-held match
    /// as soon as the bound is reached (SPEC §2.9.1: `D1 low >=10ms`).
    pub fn min_len_satisfying(self, threshold: f64) -> u64 {
        debug_assert!(self.is_lower());
        const EPS: f64 = 1e-6;
        match self {
            Cmp::Ge => (threshold - EPS).ceil().max(0.0) as u64,
            Cmp::Gt => (threshold + EPS).floor() as u64 + 1,
            _ => unreachable!("min_len_satisfying is only defined for lower bounds"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDir {
    Rising,
    Falling,
    /// `edge`: either direction (digital-only keyword).
    Either,
}

/// `D0 f=1000 100010x101` (SPEC §2.9.1, §2.9.2). Matched over the runs of
/// one digital bit, edge-anchored: a run of duration `d` samples covers
/// `round(d / cell_samples)` bit cells.
#[derive(Debug, Clone)]
pub struct BitSeqPattern {
    pub bit: u8,
    pub cell_samples: f64,
    pub tol_cells: f64,
    pub bits: Vec<BitVal>,
}

/// `D1 clk=D0 rising 1010x` (SPEC §2.9.1, SPEC §15 S-02): `data_bit`
/// sampled at each `clk_dir` edge of `clk_bit`. No frequency, no tolerance —
/// each cell's value is exactly the data bit's value the instant the clock
/// edge lands, so there is nothing to glitch-absorb.
#[derive(Debug, Clone)]
pub struct ClockedPattern {
    pub data_bit: u8,
    pub clk_bit: u8,
    pub clk_dir: EdgeDir,
    pub bits: Vec<BitVal>,
}

/// `D2 rising` / `falling` / `edge`.
#[derive(Debug, Clone, Copy)]
pub struct EdgePattern {
    pub bit: u8,
    pub dir: EdgeDir,
}

/// `D1 high >2ms <5ms` / `D1 low >=10ms` (SPEC §2.9.1). Both bounds present
/// needs the pulse's closing edge to confirm the width; a lower bound alone
/// is a level held, decidable as soon as the bound is reached.
#[derive(Debug, Clone, Copy)]
pub struct PulsePattern {
    pub bit: u8,
    /// The level being timed: `true` = high pulse, `false` = low pulse.
    pub high: bool,
    pub min: Option<(Cmp, f64)>,
    pub max: Option<(Cmp, f64)>,
}

/// `D3..D0 = 1x01` / `D0,D2 = 10` (SPEC §2.9.1): fires when the masked bits
/// of the packed byte become equal to `expected` (a transition into the
/// state, not merely holding it — see `docs` note in `digital::state`).
#[derive(Debug, Clone, Copy)]
pub struct StatePattern {
    pub mask: u8,
    pub expected: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireOn {
    Rising,
    Falling,
    Both,
}

/// `CH1 rising 1.2V` / `CH1 >1.5V` (SPEC §2.9.1), unified: `>`/`>=` is the
/// same event as `rising` — entering the "above threshold" state — and
/// `<`/`<=` the same as `falling`. Matched with hysteresis so noise at the
/// threshold does not produce a burst of crossings (see `analog` module).
#[derive(Debug, Clone, Copy)]
pub struct AnalogPattern {
    pub channel_index: u8,
    /// Raw ADC code, compile-time converted from the pattern's volts via
    /// the channel calibration.
    pub threshold_code: f64,
    /// Half-width of the hysteresis band around `threshold_code`, in codes.
    pub hyst_code: f64,
    pub fire_on: FireOn,
    /// Copied from the `RecordingInfo` given to [`crate::parse`], so
    /// [`crate::Matcher::new`] needs only the [`Pattern`] to decode the
    /// interleaved sample stream `feed_analog` is given.
    pub sample_bits: u8,
    pub channel_count: u8,
}

/// A parsed, unit-resolved pattern (SPEC §2.9.1). See [`crate::parse`].
#[derive(Debug, Clone)]
pub enum Pattern {
    BitSeq(BitSeqPattern),
    Clocked(ClockedPattern),
    Edge(EdgePattern),
    Pulse(PulsePattern),
    State(StatePattern),
    Analog(AnalogPattern),
}

impl Pattern {
    /// The packed-byte bits this pattern reads, for the storage-pruning
    /// hook (SPEC §2.8.3): a summary bucket whose runs merge to the same
    /// value under this mask cannot contain an edge the pattern cares
    /// about. `None` for analog patterns.
    pub fn mask(&self) -> Option<u8> {
        match self {
            Pattern::BitSeq(p) => Some(1 << p.bit),
            Pattern::Clocked(p) => Some((1 << p.data_bit) | (1 << p.clk_bit)),
            Pattern::Edge(p) => Some(1 << p.bit),
            Pattern::Pulse(p) => Some(1 << p.bit),
            Pattern::State(p) => Some(p.mask),
            Pattern::Analog(_) => None,
        }
    }

    pub fn is_analog(&self) -> bool {
        matches!(self, Pattern::Analog(_))
    }
}

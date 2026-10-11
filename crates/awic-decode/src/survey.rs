//! "Which pin does what" in one pass over a digital capture or recording
//! range (live test F-04): per channel its idle level, edges, pulse widths,
//! glitches and burst rhythm, plus protocol guesses — a UART line and its
//! baud rate, an I²C clock/data pair, an SPI bus, a free-running clock.
//!
//! Streaming and bounded: [`Surveyor::push`] takes transitions in order as
//! they are read, keeping counters and at most [`MAX_WIDTHS`] pulse widths
//! per channel, so a survey over minutes of a recording costs no more memory
//! than one over a second.
//!
//! How I²C is recognized: on a real bus SDA only changes while SCL is low,
//! except for START (SDA falls while SCL is high) and STOP (SDA rises while
//! SCL is high). For every ordered pair of channels the surveyor counts the
//! second one's edges that fall while the first one is high; for SCL→SDA
//! those are exactly the STARTs and STOPs — few, paired, and far fewer than
//! the clock's edges — while for any other pairing about half of all edges
//! land there. Both lines must also rest high (open drain).
//!
//! How SPI is recognized: a data line changes at most once per clock cycle,
//! right at (or just after) one of the clock's two edges — the shift edge —
//! so that it is steady at the other, the sampling edge. For every ordered
//! pair the surveyor notes, at each of the second line's edges, which level
//! the first one is at (after its own edge, if they coincide) and how long
//! ago the first one last changed. Inside a clock burst — within twice a
//! clock pulse of a clock edge — a data line's edges all fall on one side
//! of the clock; an unrelated line's split between both, and one that
//! changes more than once per clock pulse is no data line either. The side
//! gives the sampling edge, the clock's resting level CPOL, and the two the
//! SPI mode. A chip select is a quieter line the clock only runs while it
//! is away from its resting level.

use crate::baud::{estimate_from_widths, BaudEstimate, GLITCH_SAMPLES};
use crate::Transition;

/// Pulse widths kept per channel for the median, minimum and baud estimate.
pub const MAX_WIDTHS: usize = 100_000;
/// Burst start times kept per channel for the burst period.
const MAX_BURST_STARTS: usize = 2_000;
/// Largest share of SDA edges that may fall while SCL is high (those are the
/// STARTs and STOPs).
const I2C_MAX_BOUNDARY_SHARE: f64 = 0.4;
/// Clean pulses of each polarity a line needs before it can be called UART.
const MIN_PULSES_PER_LEVEL: usize = 4;
/// How close to the median width (relative) a pulse must be to count as
/// "alike" for the clock test.
const REGULAR_SPREAD: f64 = 0.05;
/// A gap between bursts is at least this many median excursions from the
/// resting level long, as well as over 1 ms: a slow pulse train is one burst, not one burst per edge (B-79).
const BURST_GAP_PULSES: u64 = 4;
/// Least share of a data line's edges inside clock bursts that fall on one
/// side of the clock.
const SPI_MIN_SIDE_SHARE: f64 = 0.9;
/// Clock edges a line must see to be taken for an SPI clock.
const SPI_MIN_CLOCK_EDGES: u64 = 16;
/// An SPI clock pauses between transfers: its longest steady stretch is at
/// least this many median pulses. A counter bit or a free-running clock
/// never does, and its neighbours would otherwise pass for data lines.
const SPI_MIN_PAUSE_PULSES: u64 = 4;
/// Least share of the clock's edges that must fall while the chip select is
/// asserted.
const SPI_MIN_CS_SHARE: f64 = 0.98;
/// A chip select is released within this many clock pulses of the last
/// clock edge of a transfer.
const SPI_CS_RELEASE_PULSES: u64 = 16;
/// Largest share of the edges a line may have more than one of in the same
/// clock pulse and still be SPI data (bit transitions are one per pulse).
const SPI_MAX_REPEAT_SHARE: f64 = 0.05;
/// Buckets for the time since the clock last changed: bucket `b` holds
/// times in `[2^(b-1), 2^b)` samples, bucket 0 a coincident edge.
const ELAPSED_BUCKETS: usize = 65;

/// Least share of a line's resting stretches that are as short as a glitch
/// for its glitches to be a signal at the sampling limit rather than spikes
/// out of a resting line (crosstalk).
const UNDERSAMPLED_MIN_SHORT_REST_SHARE: f64 = 0.5;
/// A line at the sampling limit has more than this many times the edges of
/// every other line: crosstalk never has more edges than its source.
const UNDERSAMPLED_MIN_EDGE_RATIO: u64 = 2;

/// Largest share of a line's pulses that may have one and the same length in
/// bits for it to still read as UART characters. Text has runs of many
/// lengths; a pulse train or a clock burst has nearly one (B-80).
const UART_MAX_SAME_LENGTH_SHARE: f64 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// No edge at all.
    Constant,
    /// Only pulses of ≤ [`GLITCH_SAMPLES`] samples: crosstalk or a floating input.
    GlitchesOnly,
    /// Only pulses of ≤ [`GLITCH_SAMPLES`] samples too, but back to back in
    /// runs and far more of them than any other line has edges — so no
    /// crosstalk from one: a signal at the sampling limit, too fast to time
    /// (B-100: a 5.5 MHz SPI clock at 16 MS/s).
    Undersampled,
    /// Edges in groups separated by idle gaps of more than 1 ms — including
    /// a single group with a quiet stretch that long before or after it.
    Bursty,
    /// Edges throughout, no idle gap of more than 1 ms.
    Continuous,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelSurvey {
    pub bit: u8,
    pub edges: u64,
    /// Pulses of ≤ [`GLITCH_SAMPLES`] samples (counted as two edges each above).
    pub glitches: u64,
    pub start_level: bool,
    pub end_level: bool,
    /// The level of the longest steady stretch — what the line rests at.
    pub idle_level: bool,
    /// Share of the surveyed time spent high.
    pub high_fraction: f64,
    /// Shortest non-glitch high and low pulses, in samples.
    pub min_high_samples: Option<u64>,
    pub min_low_samples: Option<u64>,
    /// Median non-glitch pulse width, in samples.
    pub median_pulse_samples: Option<u64>,
    pub first_edge: Option<u64>,
    pub last_edge: Option<u64>,
    pub bursts: u64,
    /// Median time between burst starts, when there are at least three
    /// bursts and they come at a steady rhythm (within 10 %).
    pub burst_period_samples: Option<f64>,
    pub activity: Activity,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Guess {
    Uart {
        channel: u8,
        estimate: BaudEstimate,
        /// Idles low: the decoder's `invert` option is needed.
        inverted: bool,
    },
    I2c {
        scl: u8,
        sda: u8,
        /// START conditions seen (≈ transactions).
        starts: u64,
        /// SCL rate inside a transaction, from the median clock pulse.
        clock_hz: Option<f64>,
    },
    Clock {
        channel: u8,
        frequency_hz: f64,
        /// Share of the period spent high, 0-1.
        duty: f64,
    },
    Spi {
        clk: u8,
        /// MOSI and MISO: which is which cannot be told from timing.
        data: Vec<u8>,
        cs: Option<u8>,
        /// The chip select is asserted high: the clock runs while it is high.
        /// Found from the clock, not from where the line rests longest — a
        /// long transfer keeps it asserted longer than the gaps between.
        cs_active_high: bool,
        /// 0-3, from the clock's resting level (CPOL) and the edge the data
        /// is steady at (CPHA).
        mode: u8,
        /// Clock rate inside a burst, from the median clock period; `None`
        /// when the clock is too fast to time.
        clock_hz: Option<f64>,
        /// The clock is at the sampling limit ([`Activity::Undersampled`]):
        /// the roles hold, but a faster capture would be needed to time it,
        /// and a decode may lose bits.
        undersampled: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Survey {
    pub start_sample: u64,
    pub end_sample: u64,
    pub channels: Vec<ChannelSurvey>,
    pub guesses: Vec<Guess>,
}

struct Lane {
    bit: u8,
    start_level: bool,
    level: bool,
    run_start: u64,
    edges: u64,
    glitches: u64,
    high_samples: u64,
    longest_high: u64,
    longest_low: u64,
    high_widths: Vec<u64>,
    low_widths: Vec<u64>,
    /// Every complete pulse in time order (bounded), for the baud estimate,
    /// which needs the order to merge a glitch back into its neighbours.
    sequence: Vec<u64>,
    first_edge: Option<u64>,
    last_edge: Option<u64>,
    bursts: u64,
    burst_starts: Vec<u64>,
}

impl Lane {
    fn new(bit: u8, level: bool, start: u64) -> Self {
        Lane {
            bit,
            start_level: level,
            level,
            run_start: start,
            edges: 0,
            glitches: 0,
            high_samples: 0,
            longest_high: 0,
            longest_low: 0,
            high_widths: Vec::new(),
            low_widths: Vec::new(),
            sequence: Vec::new(),
            first_edge: None,
            last_edge: None,
            bursts: 0,
            burst_starts: Vec::new(),
        }
    }

    /// Ends the current run at `at`; `complete` when it was bounded by edges
    /// on both sides (the first and last runs' true lengths are unknown).
    fn end_run(&mut self, at: u64, complete: bool) {
        let len = at - self.run_start;
        if self.level {
            self.high_samples += len;
            self.longest_high = self.longest_high.max(len);
        } else {
            self.longest_low = self.longest_low.max(len);
        }
        if complete {
            if len <= GLITCH_SAMPLES {
                self.glitches += 1;
            }
            let widths = if self.level {
                &mut self.high_widths
            } else {
                &mut self.low_widths
            };
            if widths.len() < MAX_WIDTHS {
                widths.push(len);
            }
            if self.sequence.len() < 2 * MAX_WIDTHS {
                self.sequence.push(len);
            }
        }
    }

    fn edge(&mut self, at: u64, level: bool, burst_gap: u64) {
        let complete = self.first_edge.is_some();
        self.end_run(at, complete);
        let new_burst = self.last_edge.is_none_or(|last| at - last > burst_gap);
        if new_burst {
            self.bursts += 1;
            if self.burst_starts.len() < MAX_BURST_STARTS {
                self.burst_starts.push(at);
            }
        }
        self.first_edge.get_or_insert(at);
        self.last_edge = Some(at);
        self.edges += 1;
        self.level = level;
        self.run_start = at;
    }

    /// Bursts re-cut with a gap of at least `gap` samples, from the kept
    /// run sequence: `None` when the sequence was capped, and the streaming
    /// count (1 ms gap) stands.
    fn bursts_with_gap(&self, gap: u64) -> Option<(u64, Vec<u64>)> {
        let first = self.first_edge?;
        if self.sequence.len() as u64 + 1 != self.edges {
            return None;
        }
        let mut at = first;
        let mut starts = vec![first];
        let mut bursts = 1;
        for &len in &self.sequence {
            let next = at + len;
            if len > gap {
                bursts += 1;
                if starts.len() < MAX_BURST_STARTS {
                    starts.push(next);
                }
            }
            at = next;
        }
        Some((bursts, starts))
    }

    fn all_widths(&self) -> Vec<u64> {
        let mut w: Vec<u64> = self
            .high_widths
            .iter()
            .chain(&self.low_widths)
            .copied()
            .collect();
        w.sort_unstable();
        w
    }
}

/// Per ordered pair (clock `a`, data `b`): `b`'s edges while `a` was high,
/// and while it was low (edges at the same sample as one of `a`'s are in
/// neither: they say nothing about order).
#[derive(Clone, Copy)]
struct PairCounts {
    falling_while_high: u64,
    rising_while_high: u64,
    while_low: u64,
    /// `b`'s edges by `a`'s level after any coincident edge of its own
    /// (`[low, high]`) and by the time since `a` last changed, bucketed.
    by_elapsed: [[u32; 2]; ELAPSED_BUCKETS],
    /// `b`'s edges that came in the same `a` pulse as its previous edge.
    repeats: u64,
    /// `a`'s edge count at `b`'s last edge: which pulse of `a` it was in.
    last_pulse: Option<u64>,
}

impl Default for PairCounts {
    fn default() -> Self {
        PairCounts {
            falling_while_high: 0,
            rising_while_high: 0,
            while_low: 0,
            by_elapsed: [[0; 2]; ELAPSED_BUCKETS],
            repeats: 0,
            last_pulse: None,
        }
    }
}

impl PairCounts {
    /// Edges (on the low side, on the high side) that came no later than
    /// `within` samples after an edge of `a` — inside its bursts.
    fn near(&self, within: u64) -> (u64, u64) {
        let mut sums = (0, 0);
        for (b, counts) in self.by_elapsed.iter().enumerate() {
            let floor = if b == 0 { 0 } else { 1u64 << (b - 1) };
            if floor > within {
                break;
            }
            sums.0 += u64::from(counts[0]);
            sums.1 += u64::from(counts[1]);
        }
        sums
    }
}

pub struct Surveyor {
    rate_hz: u64,
    start_sample: u64,
    prev: u8,
    lanes: Vec<Lane>,
    /// `pairs[i][j]`: lane `j`'s edges relative to lane `i`'s level.
    pairs: Vec<Vec<PairCounts>>,
    burst_gap: u64,
}

impl Surveyor {
    /// `bits`: the channels to report (bit numbers in the packed byte);
    /// `levels`: the packed value at `start_sample`.
    pub fn new(bits: &[u8], rate_hz: u64, start_sample: u64, levels: u8) -> Self {
        let lanes: Vec<Lane> = bits
            .iter()
            .map(|&b| Lane::new(b, (levels >> b) & 1 == 1, start_sample))
            .collect();
        let n = lanes.len();
        Surveyor {
            rate_hz,
            start_sample,
            prev: levels,
            lanes,
            pairs: vec![vec![PairCounts::default(); n]; n],
            burst_gap: (rate_hz / 1000).max(1),
        }
    }

    pub fn push(&mut self, transitions: &[Transition]) {
        for t in transitions {
            if t.sample_index < self.start_sample {
                continue;
            }
            let changed = self.prev ^ t.value;
            if changed == 0 {
                continue;
            }
            // Every lane's edge first, so a coincident edge of the clock is
            // already its last edge when the pairs look at it.
            for lane in &mut self.lanes {
                if (changed >> lane.bit) & 1 == 1 {
                    let rising = (t.value >> lane.bit) & 1 == 1;
                    lane.edge(t.sample_index, rising, self.burst_gap);
                }
            }
            for j in 0..self.lanes.len() {
                let bj = self.lanes[j].bit;
                if (changed >> bj) & 1 == 0 {
                    continue;
                }
                let rising = (t.value >> bj) & 1 == 1;
                for i in 0..self.lanes.len() {
                    if i == j {
                        continue;
                    }
                    let bi = self.lanes[i].bit;
                    let high = (t.value >> bi) & 1 == 1;
                    let p = &mut self.pairs[i][j];
                    if let Some(last) = self.lanes[i].last_edge {
                        let elapsed = t.sample_index - last;
                        let bucket = (u64::BITS - elapsed.leading_zeros()) as usize;
                        p.by_elapsed[bucket][usize::from(high)] += 1;
                        let pulse = self.lanes[i].edges;
                        if p.last_pulse == Some(pulse) {
                            p.repeats += 1;
                        }
                        p.last_pulse = Some(pulse);
                    }
                    if (changed >> bi) & 1 == 1 {
                        continue; // simultaneous edges say nothing about order
                    }
                    if !high {
                        p.while_low += 1;
                    } else if rising {
                        p.rising_while_high += 1;
                    } else {
                        p.falling_while_high += 1;
                    }
                }
            }
            self.prev = t.value;
        }
    }

    pub fn finish(mut self, end_sample: u64) -> Survey {
        for lane in &mut self.lanes {
            lane.end_run(end_sample.max(lane.run_start), false);
        }
        let mut channels: Vec<ChannelSurvey> = self
            .lanes
            .iter()
            .map(|l| channel_summary(l, self.start_sample, end_sample, self.burst_gap))
            .collect();
        for (k, channel) in channels.iter_mut().enumerate() {
            let lane = &self.lanes[k];
            let busiest_other = (0..self.lanes.len())
                .filter(|&j| j != k)
                .map(|j| self.lanes[j].edges)
                .max()
                .unwrap_or(0);
            if channel.activity == Activity::GlitchesOnly
                && lane.edges > UNDERSAMPLED_MIN_EDGE_RATIO * busiest_other
                && toggles_in_runs(lane, channel.idle_level)
            {
                channel.activity = Activity::Undersampled;
                // Its only "pulses" longer than a glitch are the pauses.
                channel.median_pulse_samples = None;
            }
        }

        let mut guesses = Vec::new();
        let mut claimed = vec![false; self.lanes.len()];

        // I²C first: its lines would otherwise also read as a clock. Every
        // data edge while the clock is high is by definition a START or a
        // STOP; on a real bus those are few (two per transaction, against at
        // least 18 clock edges and some data edges per byte), and they pair
        // up. For the reverse assignment (clock read as data) about half the
        // "data" edges fall while the other line is high, which fails the
        // share test.
        let mut pairs = Vec::new();
        for (i, row) in self.pairs.iter().enumerate() {
            for (j, p) in row.iter().enumerate() {
                let data_edges = self.lanes[j].edges;
                if i == j || data_edges == 0 {
                    continue;
                }
                let (starts, stops) = (p.falling_while_high, p.rising_while_high);
                let boundary_share = (starts + stops) as f64 / data_edges as f64;
                let both_idle_high = channels[i].idle_level && channels[j].idle_level;
                // SDA must hold while SCL rises, so data edges on the very
                // sample SCL rises at are no I²C — they are SPI data shifted
                // on that edge (mode 2 with a pulled-up MISO otherwise reads
                // as I²C, its few jittered edges as STARTs and STOPs).
                let on_rising = u64::from(p.by_elapsed[0][1]);
                if starts > 0
                    && on_rising <= starts + stops
                    && stops > 0
                    && starts.abs_diff(stops) <= starts / 2 + 1
                    && boundary_share <= I2C_MAX_BOUNDARY_SHARE
                    && self.lanes[i].edges >= 8 * starts
                    && both_idle_high
                {
                    pairs.push((boundary_share, i, j, starts));
                }
            }
        }
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, i, j, starts) in pairs {
            if claimed[i] || claimed[j] {
                continue;
            }
            claimed[i] = true;
            claimed[j] = true;
            // Median high plus median low is the median clock *period*
            // (rising edge to rising edge), right for any duty cycle. Twice
            // the median pulse assumed 50 %: a 42 %-duty 365 kHz SCL read as
            // ~333 kHz (B-38). The idle gaps between transactions are a few
            // outliers in the high widths and do not move the median.
            let clock_hz = clean_median(&self.lanes[i].high_widths)
                .zip(clean_median(&self.lanes[i].low_widths))
                .map(|(h, l)| self.rate_hz as f64 / (h + l).max(1) as f64);
            guesses.push(Guess::I2c {
                scl: self.lanes[i].bit,
                sda: self.lanes[j].bit,
                starts,
                clock_hz,
            });
        }

        // SPI next: its clock would otherwise read as a plain clock and its
        // data lines, at best, as UART.
        let mut clocks: Vec<usize> = (0..self.lanes.len()).collect();
        clocks.sort_by_key(|&i| std::cmp::Reverse(self.lanes[i].edges));
        for i in clocks {
            if claimed[i] {
                continue;
            }
            if let Some(guess) = self.spi_with_clock(i, &channels, &mut claimed) {
                guesses.push(guess);
            }
        }

        for (k, lane) in self.lanes.iter().enumerate() {
            if claimed[k] || lane.edges < 4 || channels[k].activity == Activity::GlitchesOnly {
                continue;
            }
            if let Some((frequency_hz, duty)) = regular_clock(lane, self.rate_hz) {
                claimed[k] = true;
                guesses.push(Guess::Clock {
                    channel: lane.bit,
                    frequency_hz,
                    duty,
                });
                continue;
            }
            let clean = |w: &[u64]| w.iter().filter(|&&x| x > GLITCH_SAMPLES).count();
            if clean(&lane.high_widths) < MIN_PULSES_PER_LEVEL
                || clean(&lane.low_widths) < MIN_PULSES_PER_LEVEL
            {
                continue;
            }
            if let Some(estimate) = estimate_from_widths(&lane.sequence, self.rate_hz)
                .filter(|e| has_varied_runs(&lane.sequence, self.rate_hz, e))
            {
                claimed[k] = true;
                guesses.push(Guess::Uart {
                    channel: lane.bit,
                    estimate,
                    inverted: !channels[k].idle_level,
                });
            }
        }

        Survey {
            start_sample: self.start_sample,
            end_sample,
            channels,
            guesses,
        }
    }

    /// An SPI bus clocked by lane `i`, if lines that behave like its data
    /// lines are unclaimed: claims them, and a chip select if one fits.
    fn spi_with_clock(
        &self,
        i: usize,
        channels: &[ChannelSurvey],
        claimed: &mut [bool],
    ) -> Option<Guess> {
        let clock = &self.lanes[i];
        if clock.edges < SPI_MIN_CLOCK_EDGES || channels[i].activity == Activity::GlitchesOnly {
            return None;
        }
        let undersampled = channels[i].activity == Activity::Undersampled;
        // A clock at the sampling limit has no pulse longer than a glitch
        // (the longer "pulses" are its pauses): its pulse is the median of
        // the glitch-length ones.
        let pulse = if undersampled {
            let short: Vec<u64> = clock
                .all_widths()
                .into_iter()
                .filter(|&w| w <= GLITCH_SAMPLES)
                .collect();
            median(&short)?
        } else {
            channels[i].median_pulse_samples?
        };
        if clock.longest_high.max(clock.longest_low) < SPI_MIN_PAUSE_PULSES * pulse {
            return None;
        }
        let within = 2 * pulse;
        // A chip select first: the clock runs only while it is asserted, and
        // it changes far less often than the clock. Which level is asserted
        // is the one the clock runs under — not where the line rests
        // longest: a long transfer keeps it asserted longer than the gaps
        // between transfers. And it brackets the bursts: half its edges are
        // releases right after a burst's last clock edge — a quiet line that
        // merely sits still while the clock runs (an idle UART) does not.
        // That release is also why the data test below would take it for a
        // data line, so CS is looked for first.
        let cs = (0..self.lanes.len())
            .filter(|&k| k != i && !claimed[k])
            .filter_map(|k| {
                let lane = &self.lanes[k];
                if lane.edges < 2 || lane.edges * 4 > clock.edges {
                    return None;
                }
                let (l, h) = self.pairs[i][k].near(SPI_CS_RELEASE_PULSES * pulse);
                if l + h < lane.edges / 2 {
                    return None;
                }
                let p = &self.pairs[k][i];
                let while_high = p.rising_while_high + p.falling_while_high;
                let active_high = while_high > p.while_low;
                let asserted = while_high.max(p.while_low);
                let total = while_high + p.while_low;
                (total > 0 && asserted as f64 >= SPI_MIN_CS_SHARE * total as f64).then_some((
                    asserted,
                    k,
                    active_high,
                ))
            })
            .max();
        let cs_active_high = cs.is_some_and(|c| c.2);
        let cs = cs.map(|c| c.1);

        // Per data line: its edges inside clock bursts, low side and high.
        let mut data = Vec::new();
        let (mut low, mut high) = (0u64, 0u64);
        for (j, lane) in self.lanes.iter().enumerate() {
            if j == i || claimed[j] || Some(j) == cs || lane.edges < 4 {
                continue;
            }
            let p = &self.pairs[i][j];
            let (l, h) = p.near(within);
            let inside = l + h;
            let fits = inside >= 4
                && 2 * inside >= lane.edges
                && l.max(h) as f64 >= SPI_MIN_SIDE_SHARE * inside as f64
                && lane.edges <= clock.edges / 2 + 2 * clock.bursts
                && p.repeats as f64
                    <= SPI_MAX_REPEAT_SHARE * lane.edges as f64 + clock.bursts as f64;
            if fits {
                data.push(j);
                low += l;
                high += h;
            }
        }
        if data.is_empty() {
            return None;
        }
        claimed[i] = true;
        for &j in &data {
            claimed[j] = true;
        }
        if let Some(k) = cs {
            claimed[k] = true;
        }
        // Data changing while the clock is low is read on the rising edge.
        let sample_rising = low >= high;
        let cpol = channels[i].idle_level;
        let cpha = if sample_rising { cpol } else { !cpol };
        let clock_hz = clean_median(&clock.high_widths)
            .zip(clean_median(&clock.low_widths))
            .map(|(h, l)| self.rate_hz as f64 / (h + l).max(1) as f64);
        Some(Guess::Spi {
            clk: clock.bit,
            data: data.iter().map(|&j| self.lanes[j].bit).collect(),
            cs: cs.map(|k| self.lanes[k].bit),
            cs_active_high,
            mode: u8::from(cpol) * 2 + u8::from(cpha),
            clock_hz,
            undersampled,
        })
    }
}

/// Whether a line whose pulses are all glitches toggles back and forth in
/// runs — most of its resting stretches as short as its pulses — rather than
/// spiking now and then out of a resting level, as crosstalk does.
fn toggles_in_runs(l: &Lane, idle_level: bool) -> bool {
    let rests = if idle_level {
        &l.high_widths
    } else {
        &l.low_widths
    };
    let short = rests.iter().filter(|&&w| w <= GLITCH_SAMPLES).count();
    !rests.is_empty() && short as f64 >= UNDERSAMPLED_MIN_SHORT_REST_SHARE * rests.len() as f64
}

/// Median of the widths longer than a glitch.
fn clean_median(w: &[u64]) -> Option<u64> {
    let mut c: Vec<u64> = w.iter().copied().filter(|&x| x > GLITCH_SAMPLES).collect();
    c.sort_unstable();
    median(&c)
}

/// Whether the line was quiet for longer than a burst gap before its first
/// edge or after its last (B-47): one burst inside a quiet window is a burst,
/// not a continuous signal.
fn idle_edge(l: &Lane, start: u64, end: u64, burst_gap: u64) -> bool {
    match (l.first_edge, l.last_edge) {
        (Some(first), Some(last)) => {
            first.saturating_sub(start) > burst_gap || end.saturating_sub(last) > burst_gap
        }
        _ => false,
    }
}

/// Whether the runs a UART estimate fits have more than one length in bits
/// (B-80). One plain pulse train — five 66 µs pulses after a 17 µs glitch —
/// fits "4 bits at 60408 Bd" perfectly, and a clock burst fits "1 bit"; text
/// mixes 1-bit runs with longer ones.
fn has_varied_runs(sequence: &[u64], rate_hz: u64, e: &BaudEstimate) -> bool {
    let bit = rate_hz as f64 / e.measured;
    let mut counts = std::collections::HashMap::new();
    let mut total = 0usize;
    for &w in sequence {
        let n = (w as f64 / bit).round();
        if w <= GLITCH_SAMPLES || !(1.0..=10.0).contains(&n) {
            continue; // a glitch or an idle gap
        }
        *counts.entry(n as u64).or_insert(0usize) += 1;
        total += 1;
    }
    let most = counts.values().copied().max().unwrap_or(0);
    total > 0 && (most as f64) < UART_MAX_SAME_LENGTH_SHARE * total as f64
}

fn median(sorted: &[u64]) -> Option<u64> {
    (!sorted.is_empty()).then(|| sorted[sorted.len() / 2])
}

fn channel_summary(l: &Lane, start: u64, end: u64, burst_gap: u64) -> ChannelSurvey {
    let span = end.saturating_sub(start).max(1);
    let clean: Vec<u64> = l
        .all_widths()
        .into_iter()
        .filter(|&w| w > GLITCH_SAMPLES)
        .collect();
    let min_clean = |w: &[u64]| w.iter().copied().filter(|&x| x > GLITCH_SAMPLES).min();
    let idle_level = if l.longest_high == l.longest_low {
        l.level
    } else {
        l.longest_high > l.longest_low
    };
    // Every excursion away from the resting level was a glitch: the long
    // stretches between them are the line resting, not pulses.
    let excursions = if idle_level {
        &l.low_widths
    } else {
        &l.high_widths
    };
    // The streaming count cut bursts at 1 ms gaps; a line whose own pulses
    // are that long needs a longer gap (B-79).
    // Measured on the excursions from the resting level, which are the
    // pulses themselves: the median of all widths is the idle stretch on a
    // line that only pulses now and then.
    let mut excursion_widths = excursions.clone();
    excursion_widths.sort_unstable();
    let gap = median(&excursion_widths).map_or(burst_gap, |m| burst_gap.max(BURST_GAP_PULSES * m));
    let (bursts, burst_starts) = l
        .bursts_with_gap(gap)
        .unwrap_or_else(|| (l.bursts, l.burst_starts.clone()));
    let activity = if l.edges == 0 {
        Activity::Constant
    } else if l.glitches > 0 && excursions.iter().all(|&w| w <= GLITCH_SAMPLES) {
        Activity::GlitchesOnly
    } else if bursts > 1 || idle_edge(l, start, end, gap) {
        Activity::Bursty
    } else {
        Activity::Continuous
    };
    ChannelSurvey {
        bit: l.bit,
        edges: l.edges,
        glitches: l.glitches,
        start_level: l.start_level,
        end_level: l.level,
        idle_level,
        high_fraction: l.high_samples as f64 / span as f64,
        min_high_samples: min_clean(&l.high_widths),
        min_low_samples: min_clean(&l.low_widths),
        median_pulse_samples: median(&clean),
        first_edge: l.first_edge,
        last_edge: l.last_edge,
        bursts,
        burst_period_samples: burst_period(&burst_starts),
        activity,
    }
}

/// Median spacing of burst starts, if there are enough and they are steady.
fn burst_period(starts: &[u64]) -> Option<f64> {
    if starts.len() < 3 {
        return None;
    }
    let mut d: Vec<u64> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    d.sort_unstable();
    let m = median(&d)? as f64;
    let steady = d.iter().all(|&x| (x as f64 - m).abs() <= 0.1 * m);
    steady.then_some(m)
}

/// A clock: nearly all high widths alike and nearly all low widths alike (a
/// clock gated in bursts, like a PWM, qualifies; its idle gaps are the few
/// outliers). Returns frequency and duty.
fn regular_clock(l: &Lane, rate_hz: u64) -> Option<(f64, f64)> {
    if l.high_widths.len() < 4 || l.low_widths.len() < 4 {
        return None;
    }
    let spread = |w: &[u64]| -> Option<f64> {
        let mut s = w.to_vec();
        s.sort_unstable();
        let m = median(&s)? as f64;
        let close = s
            .iter()
            .filter(|&&x| (x as f64 - m).abs() <= REGULAR_SPREAD * m + 1.0)
            .count();
        (m > GLITCH_SAMPLES as f64 && close as f64 >= 0.9 * s.len() as f64).then_some(m)
    };
    let high = spread(&l.high_widths)?;
    let low = spread(&l.low_widths)?;
    Some((rate_hz as f64 / (high + low), high / (high + low)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-bit level changes, merged into packed transitions at the end.
    #[derive(Default)]
    struct Wave {
        events: Vec<(u64, u8, bool)>,
    }

    impl Wave {
        fn set(&mut self, at: u64, bit: u8, high: bool) {
            self.events.push((at, bit, high));
        }

        fn packed(mut self, initial: u8) -> Vec<Transition> {
            self.events.sort_by_key(|e| e.0);
            let mut out = vec![Transition {
                sample_index: 0,
                value: initial,
            }];
            let mut v = initial;
            for (at, bit, high) in self.events {
                v = if high { v | 1 << bit } else { v & !(1 << bit) };
                let last = out.last_mut().expect("seeded with the initial value");
                if last.sample_index == at {
                    last.value = v;
                } else if last.value != v {
                    out.push(Transition {
                        sample_index: at,
                        value: v,
                    });
                }
            }
            out
        }
    }

    /// An I²C master on SCL/SDA writing `bytes` at `t`, half period `h`.
    fn i2c_write(w: &mut Wave, t: &mut u64, scl: u8, sda: u8, h: u64, bytes: &[u8]) {
        i2c_write_duty(w, t, scl, sda, h, h, bytes);
    }

    /// As [`i2c_write`], with SCL high for `hi` and low for `lo` samples.
    fn i2c_write_duty(w: &mut Wave, t: &mut u64, scl: u8, sda: u8, hi: u64, lo: u64, bytes: &[u8]) {
        w.set(*t, sda, false); // START
        *t += hi;
        for &byte in bytes {
            for i in 0..9 {
                w.set(*t, scl, false);
                let bit = if i < 8 {
                    (byte >> (7 - i)) & 1 == 1
                } else {
                    false
                }; // ACK
                w.set(*t + 1, sda, bit);
                *t += lo;
                w.set(*t, scl, true);
                *t += hi;
            }
        }
        w.set(*t, scl, false);
        w.set(*t + 1, sda, false);
        *t += lo;
        w.set(*t, scl, true);
        *t += hi;
        w.set(*t, sda, true); // STOP
        *t += hi;
    }

    fn uart_write(w: &mut Wave, t: &mut u64, bit: u8, samples_per_bit: u64, bytes: &[u8]) {
        for &byte in bytes {
            w.set(*t, bit, false);
            *t += samples_per_bit;
            for i in 0..8 {
                w.set(*t, bit, (byte >> i) & 1 == 1);
                *t += samples_per_bit;
            }
            w.set(*t, bit, true);
            *t += samples_per_bit * 2;
        }
    }

    /// One SPI transfer of `bytes` in `mode`, half clock period `h`: CS
    /// (active low) asserted around it, MOSI/MISO changed `delay` samples
    /// after each shift edge (0: at the same sample).
    #[allow(clippy::too_many_arguments)]
    fn spi_transfer(
        w: &mut Wave,
        t: &mut u64,
        (clk, mosi, miso, cs): (u8, u8, u8, Option<u8>),
        mode: u8,
        h: u64,
        delay: u64,
        bytes: &[(u8, u8)],
    ) {
        let cpol = mode >= 2;
        let cpha = mode % 2 == 1;
        if let Some(cs) = cs {
            w.set(*t, cs, false);
        }
        *t += h;
        let set_bits = |w: &mut Wave, at: u64, out: bool, inp: bool| {
            w.set(at + delay, mosi, out);
            w.set(at + delay, miso, inp);
        };
        for &(out, inp) in bytes {
            for i in (0..8).rev() {
                let (o, n) = ((out >> i) & 1 == 1, (inp >> i) & 1 == 1);
                if !cpha {
                    // Data before the leading edge, sampled on it.
                    set_bits(w, *t - h / 2, o, n);
                    w.set(*t, clk, !cpol);
                    *t += h;
                    w.set(*t, clk, cpol);
                    *t += h;
                } else {
                    // Shifted on the leading edge, sampled on the trailing.
                    w.set(*t, clk, !cpol);
                    set_bits(w, *t, o, n);
                    *t += h;
                    w.set(*t, clk, cpol);
                    *t += h;
                }
            }
        }
        if let Some(cs) = cs {
            w.set(*t, cs, true);
        }
        *t += h;
    }

    /// SPI on CLK=1, MOSI=2, MISO=5 and CS=7 (or none), several transfers
    /// 2 ms apart, plus UART on bit 3.
    fn spi_bench(mode: u8, cs: Option<u8>, delay: u64) -> (Vec<Transition>, u64) {
        let mut w = Wave::default();
        let mut t = 100;
        let payload = [(0x9F, 0x00), (0x00, 0xEF), (0x00, 0x40), (0xA5, 0x18)];
        for n in 0..6u64 {
            spi_transfer(&mut w, &mut t, (1, 2, 5, cs), mode, 5, delay, &payload);
            t = 100 + (n + 1) * 2_000;
        }
        uart_write(&mut w, &mut t, 3, 10, b"spi ok\r\n");
        let cpol = u8::from(mode >= 2) << 1;
        let cs_idle = cs.map_or(0, |b| 1 << b);
        (w.packed(cpol | cs_idle | 1 << 3), t + 1000)
    }

    fn spi_guess(t: &[Transition], end: u64) -> (Option<Guess>, Survey) {
        let mut s = Surveyor::new(&[0, 1, 2, 3, 4, 5, 6, 7], 1_000_000, 0, t[0].value);
        s.push(t);
        let survey = s.finish(end);
        let spi = survey
            .guesses
            .iter()
            .find(|g| matches!(g, Guess::Spi { .. }))
            .cloned();
        (spi, survey)
    }

    #[test]
    fn finds_an_spi_bus_and_its_mode() {
        for mode in 0..4u8 {
            for delay in [0, 1] {
                let (t, end) = spi_bench(mode, Some(7), delay);
                let (spi, survey) = spi_guess(&t, end);
                let Some(Guess::Spi {
                    clk,
                    data,
                    cs,
                    cs_active_high,
                    mode: found,
                    clock_hz,
                    undersampled: false,
                }) = spi
                else {
                    panic!("mode {mode} delay {delay}: no SPI in {:?}", survey.guesses);
                };
                assert_eq!(
                    (clk, data, cs, cs_active_high),
                    (1, vec![2, 5], Some(7), false),
                    "mode {mode}"
                );
                assert_eq!(found, mode, "delay {delay}");
                let hz = clock_hz.expect("a clock rate");
                assert!((hz - 100_000.0).abs() < 1.0, "{hz}");
                // The UART beside it is still found.
                assert!(
                    survey
                        .guesses
                        .iter()
                        .any(|g| matches!(g, Guess::Uart { channel: 3, .. })),
                    "{:?}",
                    survey.guesses
                );
            }
        }
    }

    /// Like the fake digital device: a ripple counter on D0-D3 (each bit
    /// changes exactly on its lower neighbour's falling edge, as SPI data
    /// would) and a 9600 Bd UART on D5. A counter never pauses, an SPI clock
    /// does; and the UART must not be taken for data next to it.
    #[test]
    fn a_ripple_counter_is_no_spi_bus() {
        let mut w = Wave::default();
        let end = 300_000u64;
        for i in 1..end {
            for bit in 0..4u8 {
                if i % (1 << bit) == 0 {
                    w.set(i, bit, (i >> bit) & 1 == 1);
                }
            }
        }
        let mut t = 1_000;
        uart_write(&mut w, &mut t, 5, 104, b"AWIC AWIC AWIC");
        let t = w.packed(1 << 5);
        let (spi, survey) = spi_guess(&t, end);
        assert!(spi.is_none(), "{:?}", survey.guesses);
        assert!(
            survey
                .guesses
                .iter()
                .any(|g| matches!(g, Guess::Uart { channel: 5, .. })),
            "{:?}",
            survey.guesses
        );
    }

    #[test]
    fn finds_spi_without_a_chip_select() {
        let (t, end) = spi_bench(0, None, 1);
        let (spi, survey) = spi_guess(&t, end);
        assert!(
            matches!(&spi, Some(Guess::Spi { clk: 1, cs: None, mode: 0, data, .. }) if data == &[2, 5]),
            "{:?}",
            survey.guesses
        );
    }

    /// The live bench, at 1 MHz: I²C on SCL=6/SDA=4 in four transactions
    /// 20 ms apart, UART on bit 3 at 100 kbaud, a 1 kHz clock on bit 0,
    /// bit 7 constant low.
    fn bench() -> (Vec<Transition>, u64) {
        let mut w = Wave::default();
        let mut t = 100;
        for frame in 0..4u8 {
            i2c_write(&mut w, &mut t, 6, 4, 15, &[0x78, 0x40, frame]);
            t = 100 + (frame as u64 + 1) * 20_000;
        }
        uart_write(&mut w, &mut t, 3, 10, b"help\r\nOK\r\n");
        let end = t + 1000;
        let mut c = 0;
        let mut high = true;
        while c < end {
            w.set(c, 0, high);
            high = !high;
            c += 500;
        }
        (w.packed(0b0101_1001), end)
    }

    #[test]
    fn finds_the_i2c_pair_the_uart_and_the_clock() {
        let (t, end) = bench();
        let mut s = Surveyor::new(&[0, 1, 2, 3, 4, 5, 6, 7], 1_000_000, 0, t[0].value);
        s.push(&t);
        let survey = s.finish(end);

        let i2c = survey.guesses.iter().find_map(|g| match g {
            Guess::I2c {
                scl, sda, starts, ..
            } => Some((*scl, *sda, *starts)),
            _ => None,
        });
        assert_eq!(i2c, Some((6, 4, 4)), "{:?}", survey.guesses);

        let clock = survey.guesses.iter().find_map(|g| match g {
            Guess::Clock {
                channel,
                frequency_hz,
                ..
            } => Some((*channel, *frequency_hz)),
            _ => None,
        });
        let (ch, f) = clock.expect("a clock on bit 0");
        assert_eq!(ch, 0);
        assert!((f - 1000.0).abs() < 1.0, "{f}");

        let uart = survey.guesses.iter().find_map(|g| match g {
            Guess::Uart {
                channel,
                estimate,
                inverted,
            } => Some((*channel, estimate.baud, *inverted)),
            _ => None,
        });
        assert_eq!(uart, Some((3, 100_000, false)), "{:?}", survey.guesses);
        // A UART line next to a clock is no SPI data line, nor an I²C pair
        // an SPI bus.
        assert!(
            !survey
                .guesses
                .iter()
                .any(|g| matches!(g, Guess::Spi { .. })),
            "{:?}",
            survey.guesses
        );

        let d7 = &survey.channels[7];
        assert_eq!(d7.activity, Activity::Constant);
        assert!(!d7.idle_level);
        assert!(survey.channels[4].idle_level, "I²C idles high");
        assert_eq!(survey.channels[6].bursts, 4);
        assert_eq!(survey.channels[6].burst_period_samples, Some(20_000.0));
    }

    #[test]
    fn chunked_input_gives_the_same_survey() {
        let (t, end) = bench();
        let mut whole = Surveyor::new(&[0, 3, 4, 6], 1_000_000, 0, t[0].value);
        whole.push(&t);
        let mut parts = Surveyor::new(&[0, 3, 4, 6], 1_000_000, 0, t[0].value);
        for chunk in t.chunks(7) {
            parts.push(chunk);
        }
        assert_eq!(whole.finish(end), parts.finish(end));
    }

    /// B-47: one burst inside a longer quiet window is still a burst. A
    /// triggered capture of an I²C bus — idle for 50 ms, a 30 ms burst, idle
    /// again — used to read `continuous` because only one burst was counted.
    #[test]
    fn a_single_burst_between_idle_stretches_is_bursty() {
        let mut w = Wave::default();
        // 1 MS/s; a 10 kHz clock from 50 ms to 80 ms of a 100 ms window.
        for k in 0..300u64 {
            w.set(50_000 + k * 100, 0, false);
            w.set(50_050 + k * 100, 0, true);
        }
        let t = w.packed(1);
        let mut s = Surveyor::new(&[0], 1_000_000, 0, 1);
        s.push(&t);
        let c = &s.finish(100_000).channels[0];
        assert_eq!(c.bursts, 1);
        assert_eq!(c.activity, Activity::Bursty, "{c:?}");

        // The same clock across the whole window is continuous.
        let mut w = Wave::default();
        for k in 0..1000u64 {
            w.set(k * 100 + 20, 0, false);
            w.set(k * 100 + 70, 0, true);
        }
        let t = w.packed(1);
        let mut s = Surveyor::new(&[0], 1_000_000, 0, 1);
        s.push(&t);
        let c = &s.finish(100_000).channels[0];
        assert_eq!(c.activity, Activity::Continuous, "{c:?}");
    }

    #[test]
    fn glitch_only_lines_are_called_out() {
        let mut w = Wave::default();
        for k in 0..10 {
            w.set(1000 + k * 5000, 0, true);
            w.set(1002 + k * 5000, 0, false);
        }
        let t = w.packed(0);
        let mut s = Surveyor::new(&[0], 1_000_000, 0, 0);
        s.push(&t);
        let survey = s.finish(60_000);
        let c = &survey.channels[0];
        assert_eq!(c.activity, Activity::GlitchesOnly, "{c:?}");
        assert_eq!(c.glitches, 10);
        assert!(!c.idle_level);
        assert_eq!(c.burst_period_samples, Some(5000.0));
        assert!(survey.guesses.is_empty(), "{:?}", survey.guesses);
    }

    /// B-38: the SCL of the bench's SSD1306 is 1.1 us high, 1.6 us low (42 %
    /// duty, 365 kHz). Twice the median pulse named it ~333 kHz; the median
    /// period is exact. Here at 12 MS/s: 14 high + 19 low samples.
    #[test]
    fn i2c_clock_comes_from_the_period_not_twice_a_pulse() {
        let mut w = Wave::default();
        let mut t = 100;
        for frame in 0..4u8 {
            i2c_write_duty(&mut w, &mut t, 6, 4, 14, 19, &[0x78, 0x40, frame]);
            t = 100 + (frame as u64 + 1) * 20_000;
        }
        let tr = w.packed(0b0101_0000);
        let mut s = Surveyor::new(&[4, 6], 12_000_000, 0, tr[0].value);
        s.push(&tr);
        let survey = s.finish(t + 1000);
        let hz = survey.guesses.iter().find_map(|g| match g {
            Guess::I2c { clock_hz, .. } => *clock_hz,
            _ => None,
        });
        let hz = hz.unwrap_or_else(|| panic!("no I2C guess: {:?}", survey.guesses));
        let want = 12_000_000.0 / 33.0;
        assert!((hz - want).abs() < want * 0.01, "{hz} vs {want}");
    }

    /// B-79: three 1 ms low pulses 1 ms apart (the ESP32 bench, 16 MS/s
    /// scaled to 1 MHz). Each level is held longer than the 1 ms burst gap,
    /// which cut the train at every edge: "6 bursts every 1.015 ms".
    #[test]
    fn a_slow_pulse_train_is_one_burst() {
        let mut w = Wave::default();
        for k in 0..3 {
            w.set(50_000 + k * 2030, 0, false);
            w.set(51_015 + k * 2030, 0, true);
        }
        let t = w.packed(1);
        let mut s = Surveyor::new(&[0], 1_000_000, 0, 1);
        s.push(&t);
        let c = &s.finish(100_000).channels[0];
        assert_eq!(c.edges, 6);
        assert_eq!(c.bursts, 1, "{c:?}");
        assert_eq!(c.burst_period_samples, None, "{c:?}");
        assert_eq!(c.activity, Activity::Bursty);
    }

    /// B-80, from the ESP32 bench at 2 MS/s: a 17 µs glitch from a pin
    /// switching to output, then five 66 µs low pulses (the measured widths).
    /// Every pulse is "4 bits at 60408 Bd", so the estimate fits 1.0 — but a
    /// line whose runs all have one length is a pulse train, not characters.
    #[test]
    fn a_plain_pulse_train_is_not_uart() {
        let mut w = Wave::default();
        let mut t = 1000;
        let mut low = true;
        for width in [34, 89_418, 135, 137, 131, 132, 131, 132, 131, 132, 130] {
            w.set(t, 0, !low);
            low = !low;
            t += width;
        }
        w.set(t, 0, !low);
        let packed = w.packed(1);
        let mut s = Surveyor::new(&[0], 2_000_000, 0, 1);
        s.push(&packed);
        let survey = s.finish(t + 100_000);
        assert!(survey.guesses.is_empty(), "{:?}", survey.guesses);
    }

    /// B-80, also from the bench (16 MS/s): an ESP32 I²C scan on a stuck bus
    /// leaves one 6.3 µs low pulse every ~9 ms on a line. Each pulse is
    /// "1 bit at 158 kBd"; a line with nothing but one-bit runs is not text.
    #[test]
    fn lone_short_pulses_are_not_uart() {
        let mut w = Wave::default();
        let mut t = 1000;
        for k in 0..40u64 {
            w.set(t, 0, false);
            w.set(t + 101, 0, true);
            // The scan's addresses take unequal times, so not a clock.
            t += 120_000 + (k * 7919) % 50_000;
        }
        let packed = w.packed(1);
        let mut s = Surveyor::new(&[0], 16_000_000, 0, 1);
        s.push(&packed);
        let survey = s.finish(t);
        assert!(
            !survey
                .guesses
                .iter()
                .any(|g| matches!(g, Guess::Uart { .. })),
            "{:?}",
            survey.guesses
        );
    }

    /// B-100, the live bench of 2026-10-10: an MCP2515 on SPI at ~5.5 MHz
    /// read at 16 MS/s — SCK D6, CS D3 (active low), MOSI D7, MISO D5, the
    /// INT line D1 — with every clock pulse 1 or 2 samples long. Edges in
    /// real time, each seen at the first sample at or after it.
    fn fast_spi_bench(with_crosstalk: bool) -> (Vec<Transition>, u64) {
        const RATE: f64 = 16e6;
        let half = 1.0 / 5.5e6 / 2.0;
        let at = |t: f64| (t * RATE).ceil() as u64;
        let mut w = Wave::default();
        let transfers: [&[(u8, u8)]; 3] = [
            &[(0xB0, 0x00), (0x00, 0x00)],
            &[(0x03, 0x00), (0x30, 0x00), (0x00, 0x08)],
            &[
                (0x40, 0x00),
                (0x20, 0x00),
                (0x00, 0x00),
                (0x00, 0x00),
                (0x00, 0x00),
                (0x08, 0x00),
                (0x9F, 0x00),
                (0x19, 0x00),
                (0x00, 0x00),
                (0x00, 0x00),
                (0x44, 0x00),
                (0x5C, 0x00),
                (0x0C, 0x00),
                (0x00, 0x00),
            ],
        ];
        // A one-sample spike coupled in from a neighbour's edge.
        let spike = |w: &mut Wave, at: u64, bit: u8| {
            w.set(at, bit, true);
            w.set(at + 1, bit, false);
        };
        let mut t = 10e-6;
        let mut n = 0usize;
        while t < 0.1 {
            let bytes = transfers[n % 3];
            n += 1;
            w.set(at(t), 3, false); // CS asserted
            if with_crosstalk {
                spike(&mut w, at(t), 4);
            }
            t += half;
            let (mut mosi, mut miso) = (false, false);
            for &(o, i) in bytes {
                for b in (0..8).rev() {
                    // Mode 0: data out after the falling edge, read on the rising.
                    let (no, ni) = ((o >> b) & 1 == 1, (i >> b) & 1 == 1);
                    if no != mosi {
                        w.set(at(t - half + 20e-9), 7, no);
                        if with_crosstalk {
                            spike(&mut w, at(t - half + 20e-9), 2);
                        }
                        mosi = no;
                    }
                    if ni != miso {
                        w.set(at(t - half + 30e-9), 5, ni);
                        miso = ni;
                    }
                    w.set(at(t), 6, true);
                    t += half;
                    w.set(at(t), 6, false);
                    t += half;
                }
            }
            w.set(at(t), 3, true);
            w.set(at(t), 7, false);
            w.set(at(t), 5, false);
            t += 50e-6;
            if n.is_multiple_of(40) || n.is_multiple_of(97) {
                // INT: low for ~70 µs now and then, as frames come in.
                w.set(at(t), 1, false);
                w.set(at(t + 70e-6), 1, true);
            }
        }
        (w.packed(1 << 3 | 1 << 1), at(t) + 100)
    }

    #[test]
    fn spi_clocked_at_the_sampling_limit_is_found() {
        let (t, end) = fast_spi_bench(false);
        let mut s = Surveyor::new(&[0, 1, 2, 3, 4, 5, 6, 7], 16_000_000, 0, t[0].value);
        s.push(&t);
        let survey = s.finish(end);
        let sck = &survey.channels[6];
        assert_eq!(sck.activity, Activity::Undersampled, "{sck:?}");
        let spi = survey
            .guesses
            .iter()
            .find(|g| matches!(g, Guess::Spi { .. }));
        let Some(Guess::Spi {
            clk,
            data,
            cs,
            cs_active_high,
            mode,
            clock_hz,
            undersampled,
        }) = spi
        else {
            panic!("no SPI in {:?}", survey.guesses);
        };
        assert_eq!(
            (*clk, data.as_slice(), *cs, *cs_active_high),
            (6, &[5, 7][..], Some(3), false)
        );
        assert!(*undersampled);
        assert_eq!(*mode, 0);
        assert_eq!(*clock_hz, None, "a clock that fast cannot be timed");
        // Neither CS nor INT is anything else.
        assert_eq!(survey.guesses.len(), 1, "{:?}", survey.guesses);
    }

    /// Spikes coupled in from CS (D4) and MOSI (D2) stay crosstalk next to a
    /// clock at the sampling limit, and do not hide it. (A line that picks up
    /// every edge of the clock itself cannot be told from it by timing: both
    /// then stay `GlitchesOnly`.)
    #[test]
    fn crosstalk_beside_a_fast_clock_stays_crosstalk() {
        let (t, end) = fast_spi_bench(true);
        let mut s = Surveyor::new(&[0, 1, 2, 3, 4, 5, 6, 7], 16_000_000, 0, t[0].value);
        s.push(&t);
        let survey = s.finish(end);
        for bit in [2, 4] {
            let c = &survey.channels[bit];
            assert_eq!(c.activity, Activity::GlitchesOnly, "{c:?}");
        }
        assert_eq!(survey.channels[6].activity, Activity::Undersampled);
        assert!(
            survey.guesses.iter().any(|g| matches!(
                g,
                Guess::Spi {
                    clk: 6,
                    cs: Some(3),
                    ..
                }
            )),
            "{:?}",
            survey.guesses
        );
    }
}

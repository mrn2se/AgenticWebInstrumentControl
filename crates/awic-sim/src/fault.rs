//! The faults a bench can be given to hunt down.
//!
//! Each one is a defect that real boards have, injected at the waveform and
//! felt by the firmware the way it would be — a glitched clock corrupts the
//! sensor reading, a drifting oscillator garbles the console — so it can be
//! found from the outside, with the instruments, and not by reading a flag.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Fault {
    ClockGlitch,
    I2cNack,
    BaudDrift,
    SpiCsGlitch,
    WatchdogReset,
    RuntPulse,
    OscJitter,
    KeyBounce,
}

impl Fault {
    pub const ALL: [Fault; 8] = [
        Fault::ClockGlitch,
        Fault::I2cNack,
        Fault::BaudDrift,
        Fault::SpiCsGlitch,
        Fault::WatchdogReset,
        Fault::RuntPulse,
        Fault::OscJitter,
        Fault::KeyBounce,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Fault::ClockGlitch => "clock_glitch",
            Fault::I2cNack => "i2c_nack",
            Fault::BaudDrift => "baud_drift",
            Fault::SpiCsGlitch => "spi_cs_glitch",
            Fault::WatchdogReset => "watchdog_reset",
            Fault::RuntPulse => "runt_pulse",
            Fault::OscJitter => "osc_jitter",
            Fault::KeyBounce => "key_bounce",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Fault::ClockGlitch => "Glitch on the I²C clock",
            Fault::I2cNack => "Sensor drops off the bus",
            Fault::BaudDrift => "Console garbles now and then",
            Fault::SpiCsGlitch => "Flash reads fail their checksum",
            Fault::WatchdogReset => "Board reboots by itself",
            Fault::RuntPulse => "Runt pulses on the timer output",
            Fault::OscJitter => "Timer frequency wanders",
            Fault::KeyBounce => "KEY1 skips digits",
        }
    }

    /// What a person would notice: the symptom, not the cause.
    pub fn symptom(self) -> &'static str {
        match self {
            Fault::ClockGlitch => {
                "Every few seconds the temperature reading jumps to a nonsense value and back."
            }
            Fault::I2cNack => "Now and then the console logs a failed sensor read.",
            Fault::BaudDrift => {
                "The console is fine for a while, then prints garbage, then recovers — and \
                 commands typed at those times are not understood."
            }
            Fault::SpiCsGlitch => "The firmware reports flash checksum errors at random.",
            Fault::WatchdogReset => {
                "Every half minute or so the board starts over and prints its boot banner."
            }
            Fault::RuntPulse => {
                "The 555's output has occasional pulses that never reach full height."
            }
            Fault::OscJitter => {
                "The 555's period varies from cycle to cycle far more than it should."
            }
            Fault::KeyBounce => {
                "Pressing KEY1 once sometimes advances U3's display by two or more."
            }
        }
    }

    /// The cause, for when the hunt is over.
    pub fn cause(self) -> &'static str {
        match self {
            Fault::ClockGlitch => {
                "Crosstalk puts a ~90 ns dip on SCL during some sensor reads; the sensor takes it \
                 as an extra clock and shifts its answer by one bit."
            }
            Fault::I2cNack => {
                "The TMP102 at 0x48 does not acknowledge its address on about one read in twelve."
            }
            Fault::BaudDrift => {
                "The MCU runs its UART from an untrimmed RC oscillator that drifts 1.5–6.5 % fast \
                 over a 90 s cycle; past ~5 % the stop bit lands in the next start bit."
            }
            Fault::SpiCsGlitch => {
                "Chip select to the flash bounces high for ~250 ns in the middle of some reads, \
                 which aborts the read."
            }
            Fault::WatchdogReset => {
                "The main loop hangs for about a second — heartbeat and sensor polls stop — and \
                 the watchdog resets the MCU."
            }
            Fault::RuntPulse => {
                "A marginal decoupling capacitor lets the 555's output twitch high for ~35 ns: too \
                 short to reach the rail."
            }
            Fault::OscJitter => {
                "Noise on the 555's threshold input moves each half period by several percent."
            }
            Fault::KeyBounce => {
                "KEY1's debounce capacitor is missing: its contacts bounce for about a \
                 millisecond, and U3 counts every bounce as a clock."
            }
        }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for Fault {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Fault::ALL
            .into_iter()
            .find(|f| f.id() == s.trim())
            .ok_or_else(|| {
                let known: Vec<&str> = Fault::ALL.iter().map(|f| f.id()).collect();
                format!(
                    "unknown fault {s:?}; expected one of: {}, or \"mystery\"",
                    known.join(", ")
                )
            })
    }
}

/// The faults in force, as a small set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FaultSet(u16);

impl FaultSet {
    pub fn has(self, fault: Fault) -> bool {
        self.0 & (1 << fault as u16) != 0
    }

    pub fn insert(&mut self, fault: Fault) {
        self.0 |= 1 << fault as u16;
    }

    pub fn iter(self) -> impl Iterator<Item = Fault> {
        Fault::ALL.into_iter().filter(move |f| self.has(*f))
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl FromIterator<Fault> for FaultSet {
    fn from_iter<I: IntoIterator<Item = Fault>>(iter: I) -> Self {
        let mut set = FaultSet::default();
        for f in iter {
            set.insert(f);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for f in Fault::ALL {
            assert_eq!(f.id().parse::<Fault>().unwrap(), f);
        }
        assert!("nope".parse::<Fault>().unwrap_err().contains("baud_drift"));
    }

    #[test]
    fn a_set_holds_what_was_put_in() {
        let set: FaultSet = [Fault::BaudDrift, Fault::RuntPulse].into_iter().collect();
        assert!(set.has(Fault::BaudDrift));
        assert!(!set.has(Fault::I2cNack));
        assert_eq!(set.iter().count(), 2);
    }
}

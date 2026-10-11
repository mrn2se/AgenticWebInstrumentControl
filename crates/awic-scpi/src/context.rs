//! Per-connection SCPI state (SPECIFICATION.md §9/§11.2's "own its own
//! parser state"): the selected instrument, the accumulated (not-yet-
//! started) acquisition settings, the last capture handle, the current
//! recording, and the error queue. One [`Context`] per TCP connection or
//! `scpi` terminal session — never shared, unlike [`crate::host::ScpiHost`]
//! itself.

use std::collections::VecDeque;

use crate::error::{status, ScpiError};
use crate::host::AcqSettings;

/// Bound on the error queue (IEEE-488.2 instruments all have one; unbounded
/// growth from a script that never reads `SYST:ERR?` would otherwise leak
/// memory for the life of a long-running connection).
const MAX_QUEUED_ERRORS: usize = 32;

#[derive(Debug, Default)]
pub struct Context {
    pub selected_instrument: Option<String>,
    pub pending_acq: AcqSettings,
    pub last_capture: Option<String>,
    pub current_recording: Option<u32>,
    /// `SYST:COMM:ERR:INL ON` (live test B-10): a failed query answers with
    /// its error instead of staying silent, so a line-oriented client is
    /// never left waiting for a reply that IEEE 488.2 says never comes.
    pub inline_errors: bool,
    errors: VecDeque<ScpiError>,
    /// Standard Event Status Register (`*ESR?`). Bits are set as events
    /// happen and the register is **cleared by reading it**, as IEEE 488.2
    /// §11.5.1 requires — a driver that polls `*ESR?` after each command to
    /// find out whether it was accepted depends on exactly that.
    esr: u8,
    /// Standard Event Status Enable Register (`*ESE`): which `esr` bits
    /// reach the status byte's ESB summary.
    ese: u8,
    /// Service Request Enable Register (`*SRE`): which status-byte bits
    /// reach MSS. AWIC has no way to *assert* a service request — the
    /// transports are a TCP stream and a terminal, neither of which has an
    /// SRQ line — so this register is honoured in `*STB?`'s MSS bit and
    /// nowhere else.
    sre: u8,
    /// Operation Status and Questionable Status enables (`STAT:OPER:ENAB`,
    /// `STAT:QUES:ENAB`). Stored so a driver's preset sequence round-trips;
    /// the condition and event registers they gate are always zero, because
    /// AWIC defines no operation or questionable conditions — a device
    /// failure is an error queue entry, not a status bit.
    oper_enable: u16,
    ques_enable: u16,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// `*RST` (pragmatic scope, module doc in `engine.rs`): drops
    /// session-level state, leaving the error queue alone (`*CLS`'s job).
    pub fn reset(&mut self) {
        self.selected_instrument = None;
        self.pending_acq = AcqSettings::default();
        self.last_capture = None;
        self.current_recording = None;
        self.inline_errors = false;
    }

    pub fn queue_error(&mut self, err: ScpiError) {
        self.esr |= err.esr_bit();
        if self.errors.len() >= MAX_QUEUED_ERRORS {
            self.errors.pop_front();
        }
        self.errors.push_back(err);
    }

    /// `*ESR?`: the register's value, and clear it (a destructive read, per
    /// IEEE 488.2 §11.5.1.2).
    pub fn take_esr(&mut self) -> u8 {
        std::mem::take(&mut self.esr)
    }

    /// `*OPC`: everything this engine executes has already finished by the
    /// time the next command is parsed, so the bit is set at once rather
    /// than when some background operation ends.
    pub fn set_operation_complete(&mut self) {
        self.esr |= status::OPC;
    }

    pub fn ese(&self) -> u8 {
        self.ese
    }

    pub fn set_ese(&mut self, value: u8) {
        self.ese = value;
    }

    pub fn sre(&self) -> u8 {
        self.sre
    }

    pub fn set_sre(&mut self, value: u8) {
        self.sre = value;
    }

    pub fn oper_enable(&self) -> u16 {
        self.oper_enable
    }

    pub fn set_oper_enable(&mut self, value: u16) {
        self.oper_enable = value;
    }

    pub fn ques_enable(&self) -> u16 {
        self.ques_enable
    }

    pub fn set_ques_enable(&mut self, value: u16) {
        self.ques_enable = value;
    }

    /// `STAT:PRES`: the SCPI-99 preset — the status-structure enables it
    /// defines are cleared, and the IEEE 488.2 registers (`*ESE`, `*SRE`)
    /// are deliberately left alone, which is what the standard says.
    pub fn status_preset(&mut self) {
        self.oper_enable = 0;
        self.ques_enable = 0;
    }

    /// `*STB?` (IEEE 488.2 §11.2). MSS summarises the other bits through
    /// `*SRE`, and is never enabled by itself.
    pub fn status_byte(&self) -> u8 {
        let mut stb = 0;
        if !self.errors.is_empty() {
            stb |= status::EAV;
        }
        if self.esr & self.ese != 0 {
            stb |= status::ESB;
        }
        if stb & self.sre & !status::MSS != 0 {
            stb |= status::MSS;
        }
        stb
    }

    /// `SYST:ERR?`: pop the oldest queued error, or "No error" (code 0) if
    /// the queue is empty — the standard SCPI idiom.
    pub fn pop_error(&mut self) -> (i32, String) {
        match self.errors.pop_front() {
            Some(err) => (err.code(), err.message()),
            None => (0, "No error".to_string()),
        }
    }

    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// `*CLS`: clears the error queue **and** the Standard Event Status
    /// Register, which IEEE 488.2 §10.3 makes one operation. The enable
    /// registers survive it.
    pub fn clear_errors(&mut self) {
        self.errors.clear();
        self.esr = 0;
    }

    /// Removes and returns every currently queued error, oldest first. Used
    /// by the `scpi` terminal source (unlike the TCP transport) to show
    /// errors inline as they happen rather than making a human poll
    /// `SYST:ERR?` — see `awic-server::scpi::terminal_source`.
    pub fn drain_errors(&mut self) -> Vec<ScpiError> {
        self.errors.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_reports_no_error() {
        let mut ctx = Context::new();
        assert_eq!(ctx.pop_error(), (0, "No error".to_string()));
    }

    #[test]
    fn errors_pop_in_fifo_order() {
        let mut ctx = Context::new();
        ctx.queue_error(ScpiError::UndefinedHeader("A".into()));
        ctx.queue_error(ScpiError::SyntaxError("B".into()));
        assert_eq!(ctx.error_count(), 2);
        assert_eq!(ctx.pop_error().0, -113);
        assert_eq!(ctx.pop_error().0, -102);
        assert_eq!(ctx.pop_error(), (0, "No error".to_string()));
    }

    #[test]
    fn reset_clears_session_state_but_not_errors() {
        let mut ctx = Context::new();
        ctx.selected_instrument = Some("1-1".into());
        ctx.queue_error(ScpiError::SyntaxError("x".into()));
        ctx.reset();
        assert!(ctx.selected_instrument.is_none());
        assert_eq!(ctx.error_count(), 1, "*RST must not clear the error queue");
    }
}

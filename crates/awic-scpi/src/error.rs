//! SCPI errors (SPECIFICATION.md §9): the standard negative codes an
//! IEEE-488.2 client already knows how to interpret, plus positive
//! device-specific codes carrying the server's own message verbatim.

/// One error, queued behind `SYST:ERR?`/`SYST:ERR:COUNT?`. `Display` renders
/// the code and message the way `SYST:ERR?` reports them, minus the leading
/// number (the engine formats `<code>,"<message>"` itself so it controls the
/// exact wire form in one place).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ScpiError {
    /// -100: a catch-all for a malformed command that doesn't fit a more
    /// specific code below.
    #[error("Command error: {0}")]
    CommandError(String),
    /// -102: the input could not be tokenized at all (unterminated string,
    /// malformed channel list, ...).
    #[error("Syntax error: {0}")]
    SyntaxError(String),
    /// -108: more parameters were given than the command accepts.
    #[error("Parameter not allowed: {0}")]
    ParameterNotAllowed(String),
    /// -109: a required parameter was not given.
    #[error("Missing parameter")]
    MissingParameter,
    /// -113: the header does not match any known command.
    #[error("Undefined header: {0}")]
    UndefinedHeader(String),
    /// -222: a numeric parameter parsed fine but is out of the command's
    /// acceptable range (e.g. a port or channel number of 0).
    #[error("Data out of range: {0}")]
    DataOutOfRange(String),
    /// -224: a parameter's type or value is simply wrong for this command.
    #[error("Illegal parameter value: {0}")]
    IllegalParameterValue(String),
    /// A positive, device-specific code carrying the server's own error
    /// message unchanged — the USB/PSU/acquisition/recorder failures the
    /// rest of AWIC already reports with a `{code, msg}` pair (SPEC §3.1),
    /// routed into the same error queue rather than invented SCPI codes for
    /// concepts SCPI doesn't have.
    #[error("{message}")]
    Device { code: i32, message: String },
}

impl ScpiError {
    /// The numeric code exactly as `SYST:ERR?`/`SYST:ERR:COUNT?` report it.
    pub fn code(&self) -> i32 {
        match self {
            ScpiError::CommandError(_) => -100,
            ScpiError::SyntaxError(_) => -102,
            ScpiError::ParameterNotAllowed(_) => -108,
            ScpiError::MissingParameter => -109,
            ScpiError::UndefinedHeader(_) => -113,
            ScpiError::DataOutOfRange(_) => -222,
            ScpiError::IllegalParameterValue(_) => -224,
            ScpiError::Device { code, .. } => *code,
        }
    }

    /// The message half of the queued error, independent of `Display` (kept
    /// separate so `SYST:ERR?`'s `<code>,"<message>"` formatting is exactly
    /// this string quoted, not `Display`'s `"Kind: message"` prose).
    pub fn message(&self) -> String {
        match self {
            ScpiError::CommandError(m) => m.clone(),
            ScpiError::SyntaxError(m) => m.clone(),
            ScpiError::ParameterNotAllowed(m) => m.clone(),
            ScpiError::MissingParameter => "missing parameter".to_string(),
            ScpiError::UndefinedHeader(m) => m.clone(),
            ScpiError::DataOutOfRange(m) => m.clone(),
            ScpiError::IllegalParameterValue(m) => m.clone(),
            ScpiError::Device { message, .. } => message.clone(),
        }
    }

    /// Formats one `SYST:ERR?` line: `<code>,"<message>"`.
    pub fn to_wire(&self) -> String {
        format!("{},{}", self.code(), crate::value::quote(&self.message()))
    }

    /// Which Standard Event Status Register bit this error summarises, for
    /// `*ESR?` (IEEE 488.2 §11.5.1). SCPI's own numbering already carries
    /// the answer — `-1xx` are command errors, `-2xx` execution errors,
    /// `-3xx` device-dependent, `-4xx` query errors — so the mapping is the
    /// code's own decade rather than a second table to keep in step. A
    /// positive, device-specific code (the server's own `{code, msg}`
    /// failures) is device-dependent by definition.
    pub fn esr_bit(&self) -> u8 {
        match self.code() {
            c if c >= 0 => status::DDE,
            c if c > -200 => status::CME,
            c if c > -300 => status::EXE,
            c if c > -400 => status::DDE,
            _ => status::QYE,
        }
    }
}

/// IEEE 488.2 status-reporting bit masks, named as the standard names them
/// so the engine reads like §11 does.
pub mod status {
    /// Standard Event Status Register: operation complete.
    pub const OPC: u8 = 0b0000_0001;
    /// Standard Event Status Register: query error.
    pub const QYE: u8 = 0b0000_0100;
    /// Standard Event Status Register: device-dependent error.
    pub const DDE: u8 = 0b0000_1000;
    /// Standard Event Status Register: execution error.
    pub const EXE: u8 = 0b0001_0000;
    /// Standard Event Status Register: command error.
    pub const CME: u8 = 0b0010_0000;

    /// Status byte: an error or event is queued (SCPI calls it EAV).
    pub const EAV: u8 = 0b0000_0100;
    /// Status byte: the Standard Event Status summary.
    pub const ESB: u8 = 0b0010_0000;
    /// Status byte: master summary status. Never itself enabled by `*SRE`,
    /// so it is masked out when the summary is computed.
    pub const MSS: u8 = 0b0100_0000;
}

/// One host (server-side) operation's failure, translated into a positive
/// [`ScpiError::Device`] by the engine. Kept separate from [`ScpiError`] so
/// `awic-scpi` itself never invents a code for something the server already
/// names — the server picks `code`, the engine just queues it.
#[derive(Debug, Clone, PartialEq)]
pub struct HostError {
    pub code: i32,
    pub message: String,
}

impl HostError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<HostError> for ScpiError {
    fn from(err: HostError) -> Self {
        ScpiError::Device {
            code: err.code,
            message: err.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_codes_match_the_spec_table() {
        assert_eq!(ScpiError::CommandError("x".into()).code(), -100);
        assert_eq!(ScpiError::SyntaxError("x".into()).code(), -102);
        assert_eq!(ScpiError::ParameterNotAllowed("x".into()).code(), -108);
        assert_eq!(ScpiError::MissingParameter.code(), -109);
        assert_eq!(ScpiError::UndefinedHeader("x".into()).code(), -113);
        assert_eq!(ScpiError::DataOutOfRange("x".into()).code(), -222);
        assert_eq!(ScpiError::IllegalParameterValue("x".into()).code(), -224);
    }

    #[test]
    fn device_errors_carry_the_hosts_code_and_message_unchanged() {
        let err: ScpiError = HostError::new(42, "port 3-3 port 4 feeds a hub").into();
        assert_eq!(err.code(), 42);
        assert_eq!(err.message(), "port 3-3 port 4 feeds a hub");
    }

    #[test]
    fn wire_form_is_code_comma_quoted_message() {
        let err = ScpiError::UndefinedHeader("FOO:BAR".into());
        assert_eq!(err.to_wire(), r#"-113,"FOO:BAR""#);
    }
}

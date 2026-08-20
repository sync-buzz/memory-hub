use std::process::ExitCode;

/// Stable process exit codes exposed by the bootstrap CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Code {
    Success = 0,
    Usage = 2,
    DoctorFailed = 10,
    TransportFailed = 12,
    NonFastForward = 13,
    AuthFailed = 14,
    Internal = 70,
}

impl From<Code> for ExitCode {
    fn from(code: Code) -> Self {
        Self::from(code as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::Code;

    #[test]
    fn public_exit_codes_do_not_drift() {
        assert_eq!(Code::Success as u8, 0);
        assert_eq!(Code::Usage as u8, 2);
        assert_eq!(Code::DoctorFailed as u8, 10);
        assert_eq!(Code::TransportFailed as u8, 12);
        assert_eq!(Code::NonFastForward as u8, 13);
        assert_eq!(Code::AuthFailed as u8, 14);
        assert_eq!(Code::Internal as u8, 70);
    }
}

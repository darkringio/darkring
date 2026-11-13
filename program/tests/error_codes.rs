use perps_program::error::PerpError::*;

#[test]
fn error_codes_are_stable() {
    let table = [
        (QuoteExpired as u32, 9001),
        (QuoteNotYetExpired as u32, 9002),
        (ProofVerificationFailed as u32, 9003),
        (InvalidInstructionData as u32, 9004),
        (InvalidShieldedPoolProgram as u32, 9005),
        (MissingMakerAuthority as u32, 9006),
        (InvalidMarkerMessage as u32, 9007),
        (InvalidMarkerData as u32, 9008),
        (HashingFailed as u32, 9009),
        (InvalidMakerAuthorityBump as u32, 9010),
        (InvalidOracleAccount as u32, 9011),
        (OracleNotFullyVerified as u32, 9012),
        (StaleOraclePrice as u32, 9013),
        (InvalidOraclePrice as u32, 9014),
    ];
    for (got, want) in table {
        assert_eq!(got, want, "error code drifted");
    }
}

use solana_program_error::ProgramError;
use spr_hasher::HasherError;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[repr(u32)]
pub enum PerpError {
    #[error("quote has expired")]
    QuoteExpired = 9001,
    #[error("quote has not yet expired")]
    QuoteNotYetExpired = 9002,
    #[error("proof verification failed")]
    ProofVerificationFailed = 9003,
    #[error("instruction data is invalid")]
    InvalidInstructionData = 9004,
    #[error("shielded-pool program account is invalid")]
    InvalidShieldedPoolProgram = 9005,
    #[error("maker-authority account is missing from the transact account list")]
    MissingMakerAuthority = 9006,
    #[error("quote transact must carry exactly one marker message")]
    InvalidMarkerMessage = 9007,
    #[error("quote marker message data must be the quote UTXO hash and the maker")]
    InvalidMarkerData = 9008,
    #[error("hashing failed")]
    HashingFailed = 9009,
    #[error("authority bump does not derive the maker's authority PDA")]
    InvalidMakerAuthorityBump = 9010,
    #[error("oracle account is not a price update of the oracle program")]
    InvalidOracleAccount = 9011,
    #[error("oracle price update is not fully verified")]
    OracleNotFullyVerified = 9012,
    #[error("oracle price is too old")]
    StaleOraclePrice = 9013,
    #[error("oracle price is not positive")]
    InvalidOraclePrice = 9014,
}

impl From<PerpError> for ProgramError {
    fn from(error: PerpError) -> Self {
        ProgramError::Custom(error as u32)
    }
}

impl From<HasherError> for PerpError {
    fn from(_: HasherError) -> Self {
        Self::HashingFailed
    }
}

/// A public input that does not encode as a field element fails hashing; a
/// proof that does not verify fails verification.
impl From<spr_program::verifier::VerifyError> for PerpError {
    fn from(error: spr_program::verifier::VerifyError) -> Self {
        match error {
            spr_program::verifier::VerifyError::PublicInputs => Self::HashingFailed,
            spr_program::verifier::VerifyError::Proof => Self::ProofVerificationFailed,
        }
    }
}

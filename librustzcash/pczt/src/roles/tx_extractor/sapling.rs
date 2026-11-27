<<<<<<< HEAD
use rand::{rand_core::UnwrapErr, rngs::SysRng};
=======
use rand_core::CryptoRng;
>>>>>>> 1b7adb51bbe7486a847d79d43100ccf50ad67c48
use sapling::{
    BatchValidator, Bundle,
    bundle::Authorized,
    circuit::{OutputVerifyingKey, SpendVerifyingKey},
};
use zcash_protocol::value::ZatBalance;

pub(super) fn verify_bundle(
    rng: impl CryptoRng,
    bundle: &Bundle<Authorized, ZatBalance>,
    spend_vk: &SpendVerifyingKey,
    output_vk: &OutputVerifyingKey,
    sighash: [u8; 32],
) -> Result<(), SaplingError> {
    let mut validator = BatchValidator::new();

    if !validator.check_bundle(bundle.clone(), sighash) {
        return Err(SaplingError::ConsensusRuleViolation);
    }

<<<<<<< HEAD
    if !validator.validate(spend_vk, output_vk, UnwrapErr(SysRng)) {
=======
    if !validator.validate(spend_vk, output_vk, rng) {
>>>>>>> 1b7adb51bbe7486a847d79d43100ccf50ad67c48
        return Err(SaplingError::InvalidProofsOrSignatures);
    }

    Ok(())
}

#[derive(Debug)]
pub enum SaplingError {
    ConsensusRuleViolation,
    Extract(sapling::pczt::TxExtractorError),
    InvalidProofsOrSignatures,
}

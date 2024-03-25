//! Proof of time implementation.

#![cfg_attr(not(feature = "std"), no_std)]
mod aes;

use core::num::NonZeroU32;
use subspace_core_primitives::{PotCheckpoints, PotOutput, PotSeed};
use std::time;
use tracing::info;

/// Proof of time error
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(thiserror::Error))]
pub enum PotError {
    /// Iterations is not multiple of number of checkpoints times two
    #[cfg_attr(
        feature = "thiserror",
        error(
            "Iterations {iterations} is not multiple of number of checkpoints {num_checkpoints} \
            times two"
        )
    )]
    NotMultipleOfCheckpoints {
        /// Slot iterations provided
        iterations: NonZeroU32,
        /// Number of checkpoints
        num_checkpoints: u32,
    },
}

/// Run PoT proving and produce checkpoints.
///
/// Returns error if `iterations` is not a multiple of checkpoints times two.
pub fn prove(seed: PotSeed, iterations: NonZeroU32) -> Result<PotCheckpoints, PotError> {
    let now = time::Instant::now();
    if iterations.get() % u32::from(PotCheckpoints::NUM_CHECKPOINTS.get() * 2) != 0 {
        return Err(PotError::NotMultipleOfCheckpoints {
            iterations,
            num_checkpoints: u32::from(PotCheckpoints::NUM_CHECKPOINTS.get()),
        });
    }

    let res = Ok(aes::create(
        seed,
        seed.key(),
        iterations.get() / u32::from(PotCheckpoints::NUM_CHECKPOINTS.get()),
    ));
    let duration = now.elapsed();
    info!("create slot time {:?}",duration);
    res
}

/// Verify checkpoint, number of iterations is set across uniformly distributed checkpoints.
///
/// Returns error if `iterations` is not a multiple of checkpoints times two.
pub fn verify(
    seed: PotSeed,
    iterations: NonZeroU32,
    checkpoints: &[PotOutput],
) -> Result<bool, PotError> {
    let now = time::Instant::now();
    let num_checkpoints = checkpoints.len() as u32;
    if iterations.get() % (num_checkpoints * 2) != 0 {
        return Err(PotError::NotMultipleOfCheckpoints {
            iterations,
            num_checkpoints,
        });
    }

    let res = Ok(aes::verify_sequential(
        seed,
        seed.key(),
        checkpoints,
        iterations.get() / num_checkpoints,
    ));
    let duration = now.elapsed();
    info!("verify slot time {:?}",duration);
    res
}

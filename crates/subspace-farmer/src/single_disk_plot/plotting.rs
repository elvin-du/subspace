use crate::single_disk_plot::{PlottingError, SectorMetadata};
use bitvec::order::Lsb0;
use bitvec::prelude::*;
use parity_scale_codec::Encode;
use rayon::prelude::*;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use subspace_core_primitives::crypto::kzg::Witness;
use subspace_core_primitives::{
    plot_sector_size, Piece, PieceIndex, PublicKey, SectorId, PIECE_SIZE,
};
use subspace_rpc_primitives::FarmerProtocolInfo;
use subspace_solving::derive_chunk_otp;
use tracing::debug;

/// Plotting status
pub enum PlottingStatus {
    /// Sector was plotted successfully
    PlottedSuccessfully,
    /// Plotting was interrupted due to shutdown
    Interrupted,
}

/// Plot a single sector, where `sector` and `sector_metadata` must be positioned correctly (seek to
/// desired offset before calling this function if necessary)
///
/// NOTE: Even though this function is async, it has blocking code inside and must be running in a
/// separate thread in order to prevent blocking an executor.
pub async fn plot_sector<GP, GPF, S, SM>(
    public_key: &PublicKey,
    sector_index: u64,
    get_piece: GP,
    shutting_down: &AtomicBool,
    farmer_protocol_info: &FarmerProtocolInfo,
    mut sector: S,
    mut sector_metadata: SM,
) -> Result<PlottingStatus, PlottingError>
where
    GP: Fn(PieceIndex) -> GPF,
    GPF: Future<Output = Result<Option<Piece>, Box<dyn std::error::Error + Send + Sync + 'static>>>,
    S: io::Write,
    SM: io::Write,
{
    let sector_id = SectorId::new(public_key, sector_index);
    let plot_sector_size = plot_sector_size(farmer_protocol_info.space_l);
    // TODO: Consider adding number of pieces in a sector to protocol info
    //  explicitly and, ideally, we need to remove 2x replication
    //  expectation from other places too
    let current_segment_index = farmer_protocol_info.total_pieces.get()
        / u64::from(farmer_protocol_info.recorded_history_segment_size)
        / u64::from(farmer_protocol_info.record_size.get())
        * 2;
    let expires_at = current_segment_index + farmer_protocol_info.sector_expiration;

    for piece_offset in (0..).take(plot_sector_size as usize / PIECE_SIZE) {
        if shutting_down.load(Ordering::Acquire) {
            debug!(
                %sector_index,
                "Instance is shutting down, interrupting plotting"
            );
            return Ok(PlottingStatus::Interrupted);
        }
        let piece_index = sector_id.derive_piece_index(
            piece_offset as PieceIndex,
            farmer_protocol_info.total_pieces,
        );

        let mut piece = get_piece(piece_index)
            .await
            .map_err(|error| PlottingError::FailedToRetrievePiece { piece_index, error })?
            .ok_or(PlottingError::PieceNotFound { piece_index })?;

        let piece_witness = match Witness::try_from_bytes(
            &piece[farmer_protocol_info.record_size.get() as usize..]
                .try_into()
                .expect(
                    "Witness must have correct size unless implementation \
                        is broken in a big way; qed",
                ),
        ) {
            Ok(piece_witness) => piece_witness,
            Err(error) => {
                // TODO: This will have to change once we pull pieces from
                //  DSN
                panic!(
                    "Failed to decode witness for piece {piece_index}, \
                    must be a bug on the node: {error:?}"
                );
            }
        };
        // TODO: We are skipping witness part of the piece or else it is not
        //  decodable
        // TODO: Last bits may not be encoded if record size is not multiple
        //  of `space_l`
        // Encode piece
        // TODO: Extract encoding into separate function reusable in
        //  farmer and otherwise
        piece[..farmer_protocol_info.record_size.get() as usize]
            .view_bits_mut::<Lsb0>()
            .chunks_mut(farmer_protocol_info.space_l.get() as usize)
            .enumerate()
            .par_bridge()
            .for_each(|(chunk_index, bits)| {
                // Derive one-time pad
                let mut otp = derive_chunk_otp(&sector_id, &piece_witness, chunk_index as u32);
                // XOR chunk bit by bit with one-time pad
                bits.iter_mut()
                    .zip(otp.view_bits_mut::<Lsb0>().iter())
                    .for_each(|(mut a, b)| {
                        *a ^= *b;
                    });
            });

        sector.write_all(&piece)?;
    }

    sector_metadata.write_all(
        &SectorMetadata {
            total_pieces: farmer_protocol_info.total_pieces,
            expires_at,
        }
        .encode(),
    )?;

    Ok(PlottingStatus::PlottedSuccessfully)
}

use crate::utils::multihash::MultihashCode;
use crate::{Node, PieceByHashRequest, PieceByHashResponse, PieceKey, ToMultihash};
use async_trait::async_trait;
use backoff::future::retry;
use backoff::ExponentialBackoff;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use libp2p::PeerId;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use subspace_core_primitives::{Piece, PieceIndex, PieceIndexHash};
use tokio::time::{sleep, timeout};
use tracing::{debug, trace, warn};

/// Defines initial duration between get_piece calls.
const GET_PIECE_INITIAL_INTERVAL: Duration = Duration::from_secs(1);
/// Defines max duration between get_piece calls.
const GET_PIECE_MAX_INTERVAL: Duration = Duration::from_secs(5);
/// Delay for getting piece from cache before resorting to archival storage
const GET_PIECE_ARCHIVAL_STORAGE_DELAY: Duration = Duration::from_secs(2);
/// Max time allocated for getting piece from DSN before attempt is considered to fail
const GET_PIECE_TIMEOUT: Duration = Duration::from_secs(5);

/// An abstraction for piece receiving.
#[async_trait]
pub trait PieceReceiver: Send + Sync {
    /// Returns optional piece from the DSN. None means - no piece was found.
    async fn get_piece(
        &self,
        piece_index: PieceIndex,
    ) -> Result<Option<Piece>, Box<dyn Error + Send + Sync + 'static>>;
}

// Defines target storage type for requets.
#[derive(Debug, Copy, Clone)]
enum StorageType {
    // L2 piece cache
    Cache,
    // L1 archival storage for pieces
    ArchivalStorage,
}

impl From<StorageType> for MultihashCode {
    fn from(storage_type: StorageType) -> Self {
        match storage_type {
            StorageType::Cache => MultihashCode::PieceIndex,
            StorageType::ArchivalStorage => MultihashCode::Sector,
        }
    }
}

#[async_trait]
pub trait PieceValidator: Sync + Send {
    async fn validate_piece(
        &self,
        source_peer_id: PeerId,
        piece_index: PieceIndex,
        piece: Piece,
    ) -> Option<Piece>;
}

/// Piece provider with cancellation and optional piece validator.
pub struct PieceProvider<'a, PV> {
    dsn_node: &'a Node,
    piece_validator: Option<PV>,
    cancelled: &'a AtomicBool,
}

impl<'a, PV: PieceValidator> PieceProvider<'a, PV> {
    pub fn new(dsn_node: &'a Node, piece_validator: Option<PV>, cancelled: &'a AtomicBool) -> Self {
        Self {
            dsn_node,
            piece_validator,
            cancelled,
        }
    }

    fn check_cancellation(&self) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        if self.cancelled.load(Ordering::Acquire) {
            debug!("Getting a piece was cancelled.");

            return Err("Getting a piece was cancelled.".into());
        }

        Ok(())
    }

    // Get from piece cache (L2) or archival storage (L1)
    async fn get_piece_from_storage(
        &self,
        piece_index: PieceIndex,
        storage_type: StorageType,
    ) -> Option<Piece> {
        let piece_index_hash = PieceIndexHash::from_index(piece_index);
        let key = piece_index_hash.to_multihash_by_code(storage_type.into());
        let piece_key = match storage_type {
            StorageType::Cache => PieceKey::Cache(piece_index_hash),
            StorageType::ArchivalStorage => PieceKey::ArchivalStorage(piece_index_hash),
        };

        let get_providers_result = self.dsn_node.get_providers(key).await;

        match get_providers_result {
            Ok(mut get_providers_stream) => {
                while let Some(provider_id) = get_providers_stream.next().await {
                    trace!(%piece_index, %provider_id, "get_providers returned an item");

                    let request_result = self
                        .dsn_node
                        .send_generic_request(provider_id, PieceByHashRequest { key: piece_key })
                        .await;

                    match request_result {
                        Ok(PieceByHashResponse { piece: Some(piece) }) => {
                            trace!(%provider_id, %piece_index, ?key, "Piece request succeeded.");

                            if let Some(validator) = &self.piece_validator {
                                return validator
                                    .validate_piece(provider_id, piece_index, piece)
                                    .await;
                            } else {
                                return Some(piece);
                            }
                        }
                        Ok(PieceByHashResponse { piece: None }) => {
                            debug!(%provider_id, %piece_index, ?key, "Piece request returned empty piece.");
                        }
                        Err(error) => {
                            warn!(%provider_id, %piece_index, ?key, ?error, "Piece request failed.");
                        }
                    }
                }
            }
            Err(err) => {
                warn!(%piece_index,?key, ?err, "get_providers returned an error");
            }
        }

        None
    }
}

#[async_trait]
impl<'a, PV: PieceValidator> PieceReceiver for PieceProvider<'a, PV> {
    async fn get_piece(
        &self,
        piece_index: PieceIndex,
    ) -> Result<Option<Piece>, Box<dyn Error + Send + Sync + 'static>> {
        trace!(%piece_index, "Piece request.");

        let backoff = ExponentialBackoff {
            initial_interval: GET_PIECE_INITIAL_INTERVAL,
            max_interval: GET_PIECE_MAX_INTERVAL,
            // Try until we get a valid piece
            max_elapsed_time: None,
            ..ExponentialBackoff::default()
        };

        retry(backoff, || async {
            self.check_cancellation()
                .map_err(backoff::Error::Permanent)?;

            // Try to pull pieces in two ways, whichever is faster
            let mut piece_attempts = [
                timeout(
                    GET_PIECE_TIMEOUT,
                    Box::pin(self.get_piece_from_storage(piece_index, StorageType::Cache))
                        as Pin<Box<dyn Future<Output = _> + Send>>,
                ),
                //TODO: verify "broken pipe" error cause
                timeout(
                    GET_PIECE_TIMEOUT,
                    Box::pin(async {
                        // Prefer cache if it can return quickly, otherwise fall back to archival storage
                        sleep(GET_PIECE_ARCHIVAL_STORAGE_DELAY).await;
                        self.get_piece_from_storage(piece_index, StorageType::ArchivalStorage)
                            .await
                    }) as Pin<Box<dyn Future<Output = _> + Send>>,
                ),
            ]
            .into_iter()
            .collect::<FuturesUnordered<_>>();

            while let Some(maybe_piece) = piece_attempts.next().await {
                if let Ok(Some(piece)) = maybe_piece {
                    trace!(%piece_index, "Got piece");
                    return Ok(Some(piece));
                }
            }

            warn!(%piece_index, "Couldn't get a piece from DSN. Retrying...");

            Err(backoff::Error::transient(
                "Couldn't get piece from DSN".into(),
            ))
        })
        .await
    }
}
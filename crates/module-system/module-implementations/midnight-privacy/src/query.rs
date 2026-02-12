//! Defines REST queries exposed by the MidnightPrivacy module, along with the relevant types.

use crate::hash::{
    blacklist_pos_from_recipient, empty_blacklist_bucket_entries, mt_default_nodes,
    recipient_from_pk_v2, sparse_default_nodes, BlacklistNodeKey, Hash32, MerkleNodeKey,
    NullifierKey, RootKey, BLACKLIST_BUCKET_SIZE, BLACKLIST_TREE_DEPTH,
};
use crate::types::PrivacyAddress;
use crate::ValueMidnightPrivacy;
use axum::routing::get;
use sov_modules_api::prelude::utoipa::openapi::OpenApi;
use sov_modules_api::prelude::{axum, serde_yaml, UnwrapInfallible};
use sov_modules_api::rest::utils::{errors, ApiResult, Path, Query as AxumQuery};
use sov_modules_api::rest::{ApiState, HasCustomRestApi};
use sov_modules_api::{ApiStateAccessor, Spec};
use std::collections::VecDeque;

/// Response for nullifier queries
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct NullifierResponse {
    /// The nullifier being queried
    pub nullifier: Hash32,
    /// Whether the nullifier has been spent
    pub is_spent: bool,
}

/// Response for listing all spent nullifiers
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct NullifiersListResponse {
    /// List of spent nullifiers (by position)
    pub nullifiers: Vec<NullifierInfoResponse>,
    /// Total count of spent nullifiers
    pub count: u64,
    /// Current nullifier tree root
    pub current_root: Hash32,
}

/// Response item for spent nullifier listing
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct NullifierInfoResponse {
    /// Position in the nullifier Merkle tree
    pub position: u64,
    /// Spent nullifier value
    pub nullifier: Hash32,
}

/// Response for deposit/note information
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct NoteInfoResponse {
    /// Position in the Merkle tree
    pub position: u64,
    /// Note commitment
    pub commitment: Hash32,
}

/// Response for listing all notes in the tree
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct NotesListResponse {
    /// List of all note commitments in the tree (by position)
    pub notes: Vec<NoteInfoResponse>,
    /// Total count of notes
    pub count: u64,
    /// Current Merkle root
    pub current_root: Hash32,
}

/// Response for withdrawal information
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct WithdrawalsListResponse {
    /// List of withdrawal nullifiers
    /// Note: We track withdrawals through NoteSpent events with the nullifier
    pub nullifiers: Vec<Hash32>,
    /// Total count
    pub count: usize,
}

/// Response for Merkle tree state
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct TreeStateResponse {
    /// Current Merkle root
    pub root: Hash32,
    /// Next available position
    pub next_position: u64,
    /// Tree depth
    pub depth: u8,
}

/// Response for historical roots
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct RootsResponse {
    /// Recent roots (in circular buffer)
    pub recent_roots: Vec<Hash32>,
    /// Window size
    pub window_size: u32,
}

/// Response for all historical roots
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct AllRootsResponse {
    /// All historical roots with their sequence numbers
    pub roots: Vec<RootEntry>,
    /// Total count
    pub count: usize,
}

/// Entry in the historical roots index
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct RootEntry {
    /// The root value
    pub root: Hash32,
    /// Sequence number (order it was first seen)
    pub seq: u64,
}

/// Query parameters for pagination
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PaginationParams {
    /// Limit the number of results
    #[serde(default)]
    pub limit: Option<usize>,
    /// Offset for pagination
    #[serde(default)]
    pub offset: Option<usize>,
    /// Reverse order (most recent first). Default: false (oldest first)
    #[serde(default)]
    pub reverse: Option<bool>,
}

/// Response for module statistics
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct StatsResponse {
    /// Total number of notes created (all commitments in tree)
    pub total_notes: u64,
    /// Total number of unique roots recorded in history
    pub total_roots_recorded: u64,
    /// Total amount deposited (transparent → shielded)
    pub total_deposited: u128,
    /// Number of deposit transactions
    pub deposit_count: u64,
    /// Total amount withdrawn (shielded → transparent)
    pub total_withdrawn: u128,
    /// Number of withdrawal transactions
    pub withdraw_count: u64,
    /// Current shielded pool balance (deposits - withdrawals)
    pub pool_balance: u128,
    /// Number of spent nullifiers (notes that have been consumed)
    pub nullifiers_spent: u64,
}

/// Response for the current deny-map (blacklist) root.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BlacklistRootResponse {
    /// Current deny-map Merkle root used by the ZK circuits.
    pub blacklist_root: Hash32,
}

/// Response for listing all pool admins.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct PoolAdminsResponse {
    /// Pool admin addresses (bech32 string form).
    pub admins: Vec<String>,
    /// Total count.
    pub count: u64,
}

/// Response for listing all frozen (blacklisted) privacy addresses.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct FrozenAddressesResponse {
    /// Frozen privacy pool addresses (bech32m string form).
    pub addresses: Vec<PrivacyAddress>,
    /// Total count.
    pub count: u64,
}

/// Response for a deny-map (blacklist) Merkle opening for a given privacy address.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BlacklistOpeningResponse {
    /// Current deny-map Merkle root used by the ZK circuits.
    pub blacklist_root: Hash32,
    /// Privacy address this opening corresponds to.
    pub privacy_address: PrivacyAddress,
    /// Internal recipient identifier used as deny-map key.
    pub recipient: Hash32,
    /// Leaf position derived from `recipient` (low `BLACKLIST_TREE_DEPTH` bits).
    pub pos: u64,
    /// Whether this recipient is currently blacklisted under the bucket.
    pub is_blacklisted: bool,
    /// Fixed-size bucket entries at `pos` (private inputs to the spend circuit).
    pub bucket_entries: [Hash32; BLACKLIST_BUCKET_SIZE],
    /// Sibling nodes (bottom-up), length == `BLACKLIST_TREE_DEPTH`.
    pub siblings: Vec<Hash32>,
}

/// Response for anchor root validation.
///
/// Checks whether a given Merkle root is a valid anchor — i.e. it exists in
/// `recent_roots` (sliding window) or `all_roots` (permanent history).
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct IsValidAnchorResponse {
    /// The root being validated.
    pub root: Hash32,
    /// Whether the root is a valid anchor.
    pub valid: bool,
}

impl<S: Spec> ValueMidnightPrivacy<S> {
    /// Check if a nullifier has been spent
    async fn route_check_nullifier(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        Path(nullifier_hex): Path<String>,
    ) -> ApiResult<NullifierResponse> {
        let nullifier_bytes = hex::decode(&nullifier_hex)
            .map_err(|e| errors::bad_request_400("Invalid hex string", e))?;

        let nullifier: Hash32 = nullifier_bytes
            .try_into()
            .map_err(|_| errors::bad_request_400("Nullifier must be 32 bytes", "Invalid length"))?;

        let is_spent = state
            .nullifier_set
            .get(&NullifierKey(nullifier), &mut accessor)
            .unwrap_infallible()
            .unwrap_or(false);

        Ok(NullifierResponse {
            nullifier,
            is_spent,
        }
        .into())
    }

    /// Get the current Merkle tree state
    async fn route_tree_state(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<TreeStateResponse> {
        let root = state
            .commitment_root
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Tree", "commitment_root"))?;

        let next_position = state
            .next_position
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let depth = state
            .commitment_tree_depth
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Tree", "commitment_tree_depth"))?;

        Ok(TreeStateResponse {
            root,
            next_position,
            depth,
        }
        .into())
    }

    /// Get all notes in the tree (all commitments)
    async fn route_list_notes(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        params: AxumQuery<PaginationParams>,
    ) -> ApiResult<NotesListResponse> {
        let root = state
            .commitment_root
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Tree", "commitment_root"))?;

        let depth = state
            .commitment_tree_depth
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Tree", "commitment_tree_depth"))?;
        let defaults = mt_default_nodes(depth);
        let default_leaf = defaults[0];

        let next_position = state
            .next_position
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let offset: usize = params.offset.unwrap_or(0);
        let limit: usize = params.limit.unwrap_or(100).min(1000); // Cap at 1000
        let reverse = params.reverse.unwrap_or(false);

        let mut notes = Vec::new();

        if reverse {
            // Most recent first: iterate backwards from (next_position - 1 - offset)
            if next_position > 0 {
                let start = if offset < next_position as usize {
                    next_position as usize - offset
                } else {
                    0
                };
                let end = start.saturating_sub(limit);

                for pos in (end..start).rev() {
                    let commitment = state
                        .commitment_nodes
                        .get(
                            &MerkleNodeKey {
                                height: 0,
                                index: pos as u64,
                            },
                            &mut accessor,
                        )
                        .unwrap_infallible()
                        .unwrap_or(default_leaf);
                    notes.push(NoteInfoResponse {
                        position: pos as u64,
                        commitment,
                    });
                }
            }
        } else {
            // Oldest first: iterate forwards from offset
            let end = (offset + limit).min(next_position as usize);
            for pos in offset..end {
                let commitment = state
                    .commitment_nodes
                    .get(
                        &MerkleNodeKey {
                            height: 0,
                            index: pos as u64,
                        },
                        &mut accessor,
                    )
                    .unwrap_infallible()
                    .unwrap_or(default_leaf);
                notes.push(NoteInfoResponse {
                    position: pos as u64,
                    commitment,
                });
            }
        }

        Ok(NotesListResponse {
            notes,
            count: next_position,
            current_root: root,
        }
        .into())
    }

    /// Get all spent nullifiers (paginated)
    async fn route_list_nullifiers(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        params: AxumQuery<PaginationParams>,
    ) -> ApiResult<NullifiersListResponse> {
        let root = state
            .nullifier_root
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Tree", "nullifier_root"))?;

        let depth = state
            .nullifier_tree_depth
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Tree", "nullifier_tree_depth"))?;
        let defaults = mt_default_nodes(depth);
        let default_leaf = defaults[0];

        let next_position = state
            .next_nullifier_position
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let offset: usize = params.offset.unwrap_or(0);
        let limit: usize = params.limit.unwrap_or(100).min(1000);
        let reverse = params.reverse.unwrap_or(false);

        let mut nullifiers = Vec::new();

        if reverse {
            if next_position > 0 {
                let start = if offset < next_position as usize {
                    next_position as usize - offset
                } else {
                    0
                };
                let end = start.saturating_sub(limit);

                for pos in (end..start).rev() {
                    let nullifier = state
                        .nullifier_nodes
                        .get(
                            &MerkleNodeKey {
                                height: 0,
                                index: pos as u64,
                            },
                            &mut accessor,
                        )
                        .unwrap_infallible()
                        .unwrap_or(default_leaf);
                    nullifiers.push(NullifierInfoResponse {
                        position: pos as u64,
                        nullifier,
                    });
                }
            }
        } else {
            let end = (offset + limit).min(next_position as usize);
            for pos in offset..end {
                let nullifier = state
                    .nullifier_nodes
                    .get(
                        &MerkleNodeKey {
                            height: 0,
                            index: pos as u64,
                        },
                        &mut accessor,
                    )
                    .unwrap_infallible()
                    .unwrap_or(default_leaf);
                nullifiers.push(NullifierInfoResponse {
                    position: pos as u64,
                    nullifier,
                });
            }
        }

        Ok(NullifiersListResponse {
            nullifiers,
            count: next_position,
            current_root: root,
        }
        .into())
    }

    /// Get recent roots (anchor window)
    async fn route_recent_roots(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<RootsResponse> {
        let recent_roots = state
            .recent_roots
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or_else(VecDeque::new);

        let window_size = state
            .root_window_size
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(100);

        Ok(RootsResponse {
            recent_roots: recent_roots.into_iter().collect(),
            window_size,
        }
        .into())
    }

    /// Get current deny-map (blacklist) root.
    async fn route_blacklist_root(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<BlacklistRootResponse> {
        let blacklist_root = state
            .blacklist_root
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or_else(crate::default_blacklist_root);

        Ok(BlacklistRootResponse { blacklist_root }.into())
    }

    /// List pool admins.
    async fn route_pool_admins(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<PoolAdminsResponse> {
        let admins = state
            .pool_admin_list
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or_default();
        let admins: Vec<String> = admins.into_iter().map(|a| a.to_string()).collect();

        Ok(PoolAdminsResponse {
            count: admins.len() as u64,
            admins,
        }
        .into())
    }

    /// List frozen (blacklisted) privacy addresses.
    async fn route_frozen_addresses(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<FrozenAddressesResponse> {
        let addresses = state
            .frozen_addresses
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or_default();

        Ok(FrozenAddressesResponse {
            count: addresses.len() as u64,
            addresses,
        }
        .into())
    }

    /// Get a deny-map (blacklist) Merkle opening for a given privacy address.
    ///
    /// Clients can use the returned `blacklist_root` (public) and `siblings` (private) to build
    /// spend proofs that demonstrate the address is *not* blacklisted (leaf=0) under the current
    /// root.
    async fn route_blacklist_opening(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        Path(addr_str): Path<String>,
    ) -> ApiResult<BlacklistOpeningResponse> {
        let privacy_address: PrivacyAddress = addr_str
            .parse::<PrivacyAddress>()
            .map_err(|e| errors::bad_request_400("Invalid privacy address", e))?;

        let domain = state
            .domain
            .get(&mut accessor)
            .unwrap_infallible()
            .ok_or_else(|| errors::not_found_404("Domain", "domain"))?;

        let recipient =
            recipient_from_pk_v2(&domain, &privacy_address.to_pk(), &privacy_address.pk_ivk());
        let pos = blacklist_pos_from_recipient(&recipient);

        let blacklist_root = state
            .blacklist_root
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or_else(crate::default_blacklist_root);

        let bucket_entries = state
            .blacklist_buckets
            .get(&pos, &mut accessor)
            .unwrap_infallible()
            .unwrap_or_else(empty_blacklist_bucket_entries);
        let is_blacklisted = bucket_entries.iter().any(|e| e == &recipient);

        let defaults = sparse_default_nodes(BLACKLIST_TREE_DEPTH);
        let depth = BLACKLIST_TREE_DEPTH as usize;
        let mut siblings: Vec<Hash32> = Vec::with_capacity(depth);
        for height in 0..depth {
            let sib_idx = (pos >> height) ^ 1;
            let key = BlacklistNodeKey {
                height: height as u8,
                index: sib_idx,
            };
            let sib = state
                .blacklist_nodes
                .get(&key, &mut accessor)
                .unwrap_infallible()
                .unwrap_or(defaults[height]);
            siblings.push(sib);
        }

        Ok(BlacklistOpeningResponse {
            blacklist_root,
            privacy_address,
            recipient,
            pos,
            is_blacklisted,
            bucket_entries,
            siblings,
        }
        .into())
    }

    /// Get module statistics
    async fn route_stats(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<StatsResponse> {
        let next_position = state
            .next_position
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let root_seq = state
            .root_seq
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let total_deposited = state
            .total_deposited
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let deposit_count = state
            .deposit_count
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let total_withdrawn = state
            .total_withdrawn
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        let withdraw_count = state
            .withdraw_count
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        // Calculate pool balance (deposits - withdrawals)
        let pool_balance = total_deposited.saturating_sub(total_withdrawn);

        // Accurate count maintained on spend (transfer or withdraw)
        let nullifiers_spent = state
            .spent_nullifier_count
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);

        Ok(StatsResponse {
            total_notes: next_position,
            total_roots_recorded: root_seq,
            total_deposited,
            deposit_count,
            total_withdrawn,
            withdraw_count,
            pool_balance,
            nullifiers_spent,
        }
        .into())
    }

    /// Check if a Merkle root is a valid anchor.
    ///
    /// A root is valid if it exists in either `recent_roots` (the sliding window
    /// used for fast mempool checks) or `all_roots` (the permanent NOMT-backed
    /// history of every root ever produced).  This mirrors the on-chain
    /// `is_valid_anchor` check performed during transaction execution.
    async fn route_is_valid_anchor(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        Path(root_hex): Path<String>,
    ) -> ApiResult<IsValidAnchorResponse> {
        let root_bytes =
            hex::decode(&root_hex).map_err(|e| errors::bad_request_400("Invalid hex string", e))?;

        let root: Hash32 = root_bytes
            .try_into()
            .map_err(|_| errors::bad_request_400("Root must be 32 bytes", "Invalid length"))?;

        // Fast path: check the recent roots sliding window first.
        let recent_roots = state
            .recent_roots
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or_else(VecDeque::new);

        let valid = if recent_roots.contains(&root) {
            true
        } else {
            // Fallback: check the permanent all_roots index.
            state
                .all_roots
                .get(&RootKey(root), &mut accessor)
                .unwrap_infallible()
                .is_some()
        };

        Ok(IsValidAnchorResponse { root, valid }.into())
    }
}

impl<S: Spec> HasCustomRestApi for ValueMidnightPrivacy<S> {
    type Spec = S;

    fn custom_rest_api(&self, state: ApiState<S>) -> axum::Router<()> {
        axum::Router::new()
            // List spent nullifiers
            .route("/nullifiers", get(Self::route_list_nullifiers))
            // Nullifier queries
            .route(
                "/nullifiers/:nullifier_hex",
                get(Self::route_check_nullifier),
            )
            // Tree state
            .route("/tree/state", get(Self::route_tree_state))
            // Anchor root validation
            .route(
                "/tree/is_valid_anchor/:root_hex",
                get(Self::route_is_valid_anchor),
            )
            // List all notes
            .route("/notes", get(Self::route_list_notes))
            // Recent roots (anchor window)
            .route("/roots/recent", get(Self::route_recent_roots))
            // Deny-map (blacklist) queries
            .route("/blacklist/root", get(Self::route_blacklist_root))
            .route("/blacklist/admins", get(Self::route_pool_admins))
            .route("/blacklist/frozen", get(Self::route_frozen_addresses))
            .route(
                "/blacklist/opening/:privacy_address",
                get(Self::route_blacklist_opening),
            )
            // Statistics
            .route("/stats", get(Self::route_stats))
            .with_state(state.with(self.clone()))
    }

    fn custom_openapi_spec(&self) -> Option<OpenApi> {
        let mut open_api: OpenApi =
            serde_yaml::from_str(include_str!("../openapi-v3.yaml")).expect("Invalid OpenAPI spec");
        // Because https://github.com/juhaku/utoipa/issues/972
        for path_item in open_api.paths.paths.values_mut() {
            path_item.extensions = None;
        }
        Some(open_api)
    }
}

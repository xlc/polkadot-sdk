// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Implements the Chain API Subsystem
//!
//! Provides access to the chain data. Every request may return an error.
//! At the moment, the implementation requires `Client` to implement `HeaderBackend`,
//! we may add more bounds in the future if we will need e.g. block bodies.
//!
//! Supported requests:
//! * Block hash to number
//! * Block hash to header
//! * Block weight (cumulative)
//! * Finalized block number to hash
//! * Last finalized block number
//! * Ancestors

#![deny(unused_crate_dependencies, unused_results)]
#![warn(missing_docs)]

use std::sync::Arc;

use futures::prelude::*;
use sc_client_api::{AuxStore, BlockPinning};
use schnellru::{ByLength, LruMap};
use sp_blockchain::HeaderBackend;

use polkadot_node_subsystem::{
	messages::ChainApiMessage, overseer, FromOrchestra, OverseerSignal, SpawnedSubsystem,
	SubsystemError, SubsystemResult,
};
use polkadot_primitives::{Block, Hash};

mod metrics;
use self::metrics::Metrics;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "parachain::chain-api";
// Should be lower than the upper limit in Substrate,
// but high enough to allow the slashing to succeed.
const MAX_PINNED_BLOCKS: u32 = 64;

/// The Chain API Subsystem implementation.
pub struct ChainApiSubsystem<Client> {
	client: Arc<Client>,
	// Maps the block hash to the number of times it was pinned.
	// The mapping is used to limit the number of pinned blocks
	// and enforce unpinning of blocks that were never unpinned explicitly.
	pinned_blocks: LruMap<Hash, usize>,
	metrics: Metrics,
}

impl<Client> ChainApiSubsystem<Client> {
	/// Create a new Chain API subsystem with the given client.
	pub fn new(client: Arc<Client>, metrics: Metrics) -> Self {
		let pinned_blocks = LruMap::new(ByLength::new(MAX_PINNED_BLOCKS));
		ChainApiSubsystem { client, metrics, pinned_blocks }
	}
}

#[overseer::subsystem(ChainApi, error = SubsystemError, prefix = self::overseer)]
impl<Client, Context> ChainApiSubsystem<Client>
where
	Client: HeaderBackend<Block> + BlockPinning<Block> + AuxStore + 'static,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = run::<Client, Context>(ctx, self)
			.map_err(|e| SubsystemError::with_origin("chain-api", e))
			.boxed();
		SpawnedSubsystem { future, name: "chain-api-subsystem" }
	}
}

#[overseer::contextbounds(ChainApi, prefix = self::overseer)]
async fn run<Client, Context>(
	mut ctx: Context,
	mut subsystem: ChainApiSubsystem<Client>,
) -> SubsystemResult<()>
where
	Client: HeaderBackend<Block> + BlockPinning<Block> + AuxStore,
{
	loop {
		match ctx.recv().await? {
			FromOrchestra::Signal(OverseerSignal::Conclude) => return Ok(()),
			FromOrchestra::Signal(OverseerSignal::ActiveLeaves(_)) => {},
			FromOrchestra::Signal(OverseerSignal::BlockFinalized(..)) => {},
			FromOrchestra::Communication { msg } => match msg {
				ChainApiMessage::BlockNumber(hash, response_channel) => {
					let _timer = subsystem.metrics.time_block_number();
					let result = subsystem.client.number(hash).map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::BlockHeader(hash, response_channel) => {
					let _timer = subsystem.metrics.time_block_header();
					let result = subsystem.client.header(hash).map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::BlockWeight(hash, response_channel) => {
					let _timer = subsystem.metrics.time_block_weight();
					let result = sc_consensus_babe::block_weight(&*subsystem.client, hash)
						.map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::FinalizedBlockHash(number, response_channel) => {
					let _timer = subsystem.metrics.time_finalized_block_hash();
					// Note: we don't verify it's finalized
					let result = subsystem.client.hash(number).map_err(|e| e.to_string().into());
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::FinalizedBlockNumber(response_channel) => {
					let _timer = subsystem.metrics.time_finalized_block_number();
					let result = subsystem.client.info().finalized_number;
					// always succeeds
					subsystem.metrics.on_request(true);
					let _ = response_channel.send(Ok(result));
				},
				ChainApiMessage::Ancestors { hash, k, response_channel } => {
					let _timer = subsystem.metrics.time_ancestors();
					gum::trace!(target: LOG_TARGET, hash=%hash, k=k, "ChainApiMessage::Ancestors");

					let mut hash = hash;

					let next_parent = core::iter::from_fn(|| {
						let maybe_header = subsystem.client.header(hash);
						match maybe_header {
							// propagate the error
							Err(e) => {
								let e = e.to_string().into();
								Some(Err(e))
							},
							// fewer than `k` ancestors are available
							Ok(None) => None,
							Ok(Some(header)) => {
								// stop at the genesis header.
								if header.number == 0 {
									None
								} else {
									hash = header.parent_hash;
									Some(Ok(hash))
								}
							},
						}
					});

					let result = next_parent.take(k).collect::<Result<Vec<_>, _>>();
					subsystem.metrics.on_request(result.is_ok());
					let _ = response_channel.send(result);
				},
				ChainApiMessage::PinBlock(hash) => {
					let _timer = subsystem.metrics.time_pin_block();

					// check if the map is full
					if subsystem.pinned_blocks.len() == MAX_PINNED_BLOCKS as usize {
						// unpin the least recently pinned block
						let (hash, count) = subsystem
							.pinned_blocks
							.pop_oldest()
							.expect("len is checked above; qed");
						for _ in 0..count {
							subsystem.client.unpin_block(hash);
						}
					}
					if let Some(count) = subsystem.pinned_blocks.get_or_insert(hash, || 0) {
						*count += 1;
					}
					// don't propagate the result
					// the caller can not do anything about it
					let result = subsystem.client.pin_block(hash);
					subsystem.metrics.on_request(result.is_ok());
				},
				ChainApiMessage::UnpinBlock(hash) => {
					let _timer = subsystem.metrics.time_unpin_block();
					if let Some(count) = subsystem.pinned_blocks.get(&hash) {
						*count = count.saturating_sub(1);
						if *count == 0 {
							let _ = subsystem.pinned_blocks.remove(&hash);
						}
						subsystem.client.unpin_block(hash);
					}
					// always succeeds
					subsystem.metrics.on_request(true);
				},
			},
		}
	}
}

// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

use crate::sync::HeadersSyncParams;
use crate::sync_types::{HeaderId, HeaderStatus, HeadersSyncPipeline, MaybeConnectionError, QueuedHeader};

use async_trait::async_trait;
use futures::{future::FutureExt, stream::StreamExt};
use num_traits::{Saturating, Zero};
use std::{
	collections::HashSet,
	time::{Duration, Instant},
};

/// When we submit headers to target node, but see no updates of best
/// source block known to target node during STALL_SYNC_TIMEOUT seconds,
/// we consider that our headers are rejected because there has been reorg in target chain.
/// This reorg could invalidate our knowledge about sync process (i.e. we have asked if
/// HeaderA is known to target, but then reorg happened and the answer is different
/// now) => we need to reset sync.
/// The other option is to receive **EVERY** best target header and check if it is
/// direct child of previous best header. But: (1) subscription doesn't guarantee that
/// the subscriber will receive every best header (2) reorg won't always lead to sync
/// stall and restart is a heavy operation (we forget all in-memory headers).
const STALL_SYNC_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Delay after we have seen update of best source header at target node,
/// for us to treat sync stalled. ONLY when relay operates in backup mode.
const BACKUP_STALL_SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Delay after connection-related error happened before we'll try
/// reconnection again.
const CONNECTION_ERROR_DELAY: Duration = Duration::from_secs(10);

/// Type alias for all SourceClient futures.
pub type OwnedSourceFutureOutput<Client, P, T> = (Client, Result<T, <Client as SourceClient<P>>::Error>);
/// Type alias for all TargetClient futures.
pub type OwnedTargetFutureOutput<Client, P, T> = (Client, Result<T, <Client as TargetClient<P>>::Error>);

/// Source client trait.
#[async_trait]
pub trait SourceClient<P: HeadersSyncPipeline>: Sized {
	/// Type of error this clients returns.
	type Error: std::fmt::Debug + MaybeConnectionError;

	/// Get best block number.
	async fn best_block_number(self) -> OwnedSourceFutureOutput<Self, P, P::Number>;

	/// Get header by hash.
	async fn header_by_hash(self, hash: P::Hash) -> OwnedSourceFutureOutput<Self, P, P::Header>;

	/// Get canonical header by number.
	async fn header_by_number(self, number: P::Number) -> OwnedSourceFutureOutput<Self, P, P::Header>;

	/// Get completion data by header hash.
	async fn header_completion(
		self,
		id: HeaderId<P::Hash, P::Number>,
	) -> OwnedSourceFutureOutput<Self, P, (HeaderId<P::Hash, P::Number>, Option<P::Completion>)>;

	/// Get extra data by header hash.
	async fn header_extra(
		self,
		id: HeaderId<P::Hash, P::Number>,
		header: QueuedHeader<P>,
	) -> OwnedSourceFutureOutput<Self, P, (HeaderId<P::Hash, P::Number>, P::Extra)>;
}

/// Target client trait.
#[async_trait]
pub trait TargetClient<P: HeadersSyncPipeline>: Sized {
	/// Type of error this clients returns.
	type Error: std::fmt::Debug + MaybeConnectionError;

	/// Returns ID of best header known to the target node.
	async fn best_header_id(self) -> OwnedTargetFutureOutput<Self, P, HeaderId<P::Hash, P::Number>>;

	/// Returns true if header is known to the target node.
	async fn is_known_header(
		self,
		id: HeaderId<P::Hash, P::Number>,
	) -> OwnedTargetFutureOutput<Self, P, (HeaderId<P::Hash, P::Number>, bool)>;

	/// Submit headers.
	async fn submit_headers(
		self,
		headers: Vec<QueuedHeader<P>>,
	) -> OwnedTargetFutureOutput<Self, P, Vec<HeaderId<P::Hash, P::Number>>>;

	/// Returns ID of headers that require to be 'completed' before children can be submitted.
	async fn incomplete_headers_ids(self) -> OwnedTargetFutureOutput<Self, P, HashSet<HeaderId<P::Hash, P::Number>>>;

	/// Submit completion data for header.
	async fn complete_header(
		self,
		id: HeaderId<P::Hash, P::Number>,
		completion: P::Completion,
	) -> OwnedTargetFutureOutput<Self, P, HeaderId<P::Hash, P::Number>>;

	/// Returns true if header requires extra data to be submitted.
	async fn requires_extra(
		self,
		header: QueuedHeader<P>,
	) -> OwnedTargetFutureOutput<Self, P, (HeaderId<P::Hash, P::Number>, bool)>;
}

/// Run headers synchronization.
pub fn run<P: HeadersSyncPipeline>(
	source_client: impl SourceClient<P>,
	source_tick: Duration,
	target_client: impl TargetClient<P>,
	target_tick: Duration,
	sync_params: HeadersSyncParams,
) {
	let mut local_pool = futures::executor::LocalPool::new();
	let mut progress_context = (Instant::now(), None, None);

	local_pool.run_until(async move {
		let mut sync = crate::sync::HeadersSync::<P>::new(sync_params);
		let mut stall_countdown = None;
		let mut last_update_time = Instant::now();

		let mut source_maybe_client = None;
		let mut source_best_block_number_required = false;
		let source_best_block_number_future = source_client.best_block_number().fuse();
		let source_new_header_future = futures::future::Fuse::terminated();
		let source_orphan_header_future = futures::future::Fuse::terminated();
		let source_extra_future = futures::future::Fuse::terminated();
		let source_completion_future = futures::future::Fuse::terminated();
		let source_go_offline_future = futures::future::Fuse::terminated();
		let source_tick_stream = interval(source_tick).fuse();

		let mut target_maybe_client = None;
		let mut target_best_block_required = false;
		let mut target_incomplete_headers_required = true;
		let target_best_block_future = target_client.best_header_id().fuse();
		let target_incomplete_headers_future = futures::future::Fuse::terminated();
		let target_extra_check_future = futures::future::Fuse::terminated();
		let target_existence_status_future = futures::future::Fuse::terminated();
		let target_submit_header_future = futures::future::Fuse::terminated();
		let target_complete_header_future = futures::future::Fuse::terminated();
		let target_go_offline_future = futures::future::Fuse::terminated();
		let target_tick_stream = interval(target_tick).fuse();

		futures::pin_mut!(
			source_best_block_number_future,
			source_new_header_future,
			source_orphan_header_future,
			source_extra_future,
			source_completion_future,
			source_go_offline_future,
			source_tick_stream,
			target_best_block_future,
			target_incomplete_headers_future,
			target_extra_check_future,
			target_existence_status_future,
			target_submit_header_future,
			target_complete_header_future,
			target_go_offline_future,
			target_tick_stream
		);

		loop {
			futures::select! {
				(source_client, source_best_block_number) = source_best_block_number_future => {
					source_best_block_number_required = false;

					process_future_result(
						&mut source_maybe_client,
						source_client,
						source_best_block_number,
						|source_best_block_number| sync.source_best_header_number_response(source_best_block_number),
						&mut source_go_offline_future,
						|source_client| delay(CONNECTION_ERROR_DELAY, source_client),
						|| format!("Error retrieving best header number from {}", P::SOURCE_NAME),
					);
				},
				(source_client, source_new_header) = source_new_header_future => {
					process_future_result(
						&mut source_maybe_client,
						source_client,
						source_new_header,
						|source_new_header| sync.headers_mut().header_response(source_new_header),
						&mut source_go_offline_future,
						|source_client| delay(CONNECTION_ERROR_DELAY, source_client),
						|| format!("Error retrieving header from {} node", P::SOURCE_NAME),
					);
				},
				(source_client, source_orphan_header) = source_orphan_header_future => {
					process_future_result(
						&mut source_maybe_client,
						source_client,
						source_orphan_header,
						|source_orphan_header| sync.headers_mut().header_response(source_orphan_header),
						&mut source_go_offline_future,
						|source_client| delay(CONNECTION_ERROR_DELAY, source_client),
						|| format!("Error retrieving orphan header from {} node", P::SOURCE_NAME),
					);
				},
				(source_client, source_extra) = source_extra_future => {
					process_future_result(
						&mut source_maybe_client,
						source_client,
						source_extra,
						|(header, extra)| sync.headers_mut().extra_response(&header, extra),
						&mut source_go_offline_future,
						|source_client| delay(CONNECTION_ERROR_DELAY, source_client),
						|| format!("Error retrieving extra data from {} node", P::SOURCE_NAME),
					);
				},
				(source_client, source_completion) = source_completion_future => {
					process_future_result(
						&mut source_maybe_client,
						source_client,
						source_completion,
						|(header, completion)| sync.headers_mut().completion_response(&header, completion),
						&mut source_go_offline_future,
						|source_client| delay(CONNECTION_ERROR_DELAY, source_client),
						|| format!("Error retrieving completion data from {} node", P::SOURCE_NAME),
					);
				},
				source_client = source_go_offline_future => {
					source_maybe_client = Some(source_client);
				},
				_ = source_tick_stream.next() => {
					if sync.is_almost_synced() {
						source_best_block_number_required = true;
					}
				},
				(target_client, target_best_block) = target_best_block_future => {
					target_best_block_required = false;

					process_future_result(
						&mut target_maybe_client,
						target_client,
						target_best_block,
						|target_best_block| {
							let head_updated = sync.target_best_header_response(target_best_block);
							if head_updated {
								last_update_time = Instant::now();
							}
							match head_updated {
								// IF head is updated AND there are still our transactions:
								// => restart stall countdown timer
								true if sync.headers().headers_in_status(HeaderStatus::Submitted) != 0 =>
									stall_countdown = Some(Instant::now()),
								// IF head is updated AND there are no our transactions:
								// => stop stall countdown timer
								true => stall_countdown = None,
								// IF head is not updated AND stall countdown is not yet completed
								// => do nothing
								false if stall_countdown
									.map(|stall_countdown| stall_countdown.elapsed() < STALL_SYNC_TIMEOUT)
									.unwrap_or(true)
									=> (),
								// IF head is not updated AND stall countdown has completed
								// => restart sync
								false => {
									log::info!(
										target: "bridge",
										"Possible {} fork detected. Restarting {} headers synchronization.",
										P::TARGET_NAME,
										P::SOURCE_NAME,
									);
									stall_countdown = None;
									sync.restart();
								},
							}
						},
						&mut target_go_offline_future,
						|target_client| delay(CONNECTION_ERROR_DELAY, target_client),
						|| format!("Error retrieving best known header from {} node", P::TARGET_NAME),
					);
				},
				(target_client, incomplete_headers_ids) = target_incomplete_headers_future => {
					target_incomplete_headers_required = false;

					process_future_result(
						&mut target_maybe_client,
						target_client,
						incomplete_headers_ids,
						|incomplete_headers_ids| sync.headers_mut().incomplete_headers_response(incomplete_headers_ids),
						&mut target_go_offline_future,
						|target_client| delay(CONNECTION_ERROR_DELAY, target_client),
						|| format!("Error retrieving incomplete headers from {} node", P::TARGET_NAME),
					);
				},
				(target_client, target_existence_status) = target_existence_status_future => {
					process_future_result(
						&mut target_maybe_client,
						target_client,
						target_existence_status,
						|(target_header, target_existence_status)| sync
							.headers_mut()
							.maybe_orphan_response(&target_header, target_existence_status),
						&mut target_go_offline_future,
						|target_client| delay(CONNECTION_ERROR_DELAY, target_client),
						|| format!("Error retrieving existence status from {} node", P::TARGET_NAME),
					);
				},
				(target_client, target_submit_header_result) = target_submit_header_future => {
					process_future_result(
						&mut target_maybe_client,
						target_client,
						target_submit_header_result,
						|submitted_headers| sync.headers_mut().headers_submitted(submitted_headers),
						&mut target_go_offline_future,
						|target_client| delay(CONNECTION_ERROR_DELAY, target_client),
						|| format!("Error submitting headers to {} node", P::TARGET_NAME),
					);
				},
				(target_client, target_complete_header_result) = target_complete_header_future => {
					process_future_result(
						&mut target_maybe_client,
						target_client,
						target_complete_header_result,
						|completed_header| sync.headers_mut().header_completed(&completed_header),
						&mut target_go_offline_future,
						|target_client| delay(CONNECTION_ERROR_DELAY, target_client),
						|| format!("Error completing headers at {}", P::TARGET_NAME),
					);
				},
				(target_client, target_extra_check_result) = target_extra_check_future => {
					process_future_result(
						&mut target_maybe_client,
						target_client,
						target_extra_check_result,
						|(header, extra_check_result)| sync
							.headers_mut()
							.maybe_extra_response(&header, extra_check_result),
						&mut target_go_offline_future,
						|target_client| delay(CONNECTION_ERROR_DELAY, target_client),
						|| format!("Error retrieving receipts requirement from {} node", P::TARGET_NAME),
					);
				},
				target_client = target_go_offline_future => {
					target_maybe_client = Some(target_client);
				},
				_ = target_tick_stream.next() => {
					target_best_block_required = true;
					target_incomplete_headers_required = true;
				},
			}

			// print progress
			progress_context = print_sync_progress(progress_context, &sync);

			// if target client is available: wait, or call required target methods
			if let Some(target_client) = target_maybe_client.take() {
				// the priority is to:
				// 1) get best block - it stops us from downloading/submitting new blocks + we call it rarely;
				// 2) get incomplete headers - it stops us from submitting new blocks + we call it rarely;
				// 3) complete headers - it stops us from submitting new blocks;
				// 4) check if we need extra data from source - it stops us from downloading/submitting new blocks;
				// 5) check existence - it stops us from submitting new blocks;
				// 6) submit header

				if target_best_block_required {
					log::debug!(target: "bridge", "Asking {} about best block", P::TARGET_NAME);
					target_best_block_future.set(target_client.best_header_id().fuse());
				} else if target_incomplete_headers_required {
					log::debug!(target: "bridge", "Asking {} about incomplete headers", P::TARGET_NAME);
					target_incomplete_headers_future.set(target_client.incomplete_headers_ids().fuse());
				} else if let Some((id, completion)) = sync.headers_mut().header_to_complete() {
					log::debug!(
						target: "bridge",
						"Going to complete header: {:?}",
						id,
					);

					target_complete_header_future.set(target_client.complete_header(id, completion.clone()).fuse());
				} else if let Some(header) = sync.headers().header(HeaderStatus::MaybeExtra) {
					log::debug!(
						target: "bridge",
						"Checking if header submission requires extra: {:?}",
						header.id(),
					);

					target_extra_check_future.set(target_client.requires_extra(header.clone()).fuse());
				} else if let Some(header) = sync.headers().header(HeaderStatus::MaybeOrphan) {
					// for MaybeOrphan we actually ask for parent' header existence
					let parent_id = header.parent_id();

					log::debug!(
						target: "bridge",
						"Asking {} node for existence of: {:?}",
						P::TARGET_NAME,
						parent_id,
					);

					target_existence_status_future.set(target_client.is_known_header(parent_id).fuse());
				} else if let Some(headers) =
					sync.select_headers_to_submit(last_update_time.elapsed() > BACKUP_STALL_SYNC_TIMEOUT)
				{
					let ids = match headers.len() {
						1 => format!("{:?}", headers[0].id()),
						2 => format!("[{:?}, {:?}]", headers[0].id(), headers[1].id()),
						len => format!("[{:?} ... {:?}]", headers[0].id(), headers[len - 1].id()),
					};
					log::debug!(
						target: "bridge",
						"Submitting {} header(s) to {} node: {:?}",
						headers.len(),
						P::TARGET_NAME,
						ids,
					);

					let headers = headers.into_iter().cloned().collect();
					target_submit_header_future.set(target_client.submit_headers(headers).fuse());

					// remember that we have submitted some headers
					if stall_countdown.is_none() {
						stall_countdown = Some(Instant::now());
					}
				} else {
					target_maybe_client = Some(target_client);
				}
			}

			// if source client is available: wait, or call required source methods
			if let Some(source_client) = source_maybe_client.take() {
				// the priority is to:
				// 1) get best block - it stops us from downloading new blocks + we call it rarely;
				// 2) download completion data - it stops us from submitting new blocks;
				// 3) download extra data - it stops us from submitting new blocks;
				// 4) download missing headers - it stops us from downloading/submitting new blocks;
				// 5) downloading new headers

				if source_best_block_number_required {
					log::debug!(target: "bridge", "Asking {} node about best block number", P::SOURCE_NAME);
					source_best_block_number_future.set(source_client.best_block_number().fuse());
				} else if let Some(id) = sync.headers_mut().incomplete_header() {
					log::debug!(
						target: "bridge",
						"Retrieving completion data for header: {:?}",
						id,
					);
					source_completion_future.set(source_client.header_completion(id).fuse());
				} else if let Some(header) = sync.headers().header(HeaderStatus::Extra) {
					let id = header.id();
					log::debug!(
						target: "bridge",
						"Retrieving extra data for header: {:?}",
						id,
					);
					source_extra_future.set(source_client.header_extra(id, header.clone()).fuse());
				} else if let Some(header) = sync.headers().header(HeaderStatus::Orphan) {
					// for Orphan we actually ask for parent' header
					let parent_id = header.parent_id();

					// if we have end up with orphan header#0, then we are misconfigured
					if parent_id.0.is_zero() {
						log::error!(
							target: "bridge",
							"Misconfiguration. Genesis {} header is considered orphan by {} node",
							P::SOURCE_NAME,
							P::TARGET_NAME,
						);
						return;
					}

					log::debug!(
						target: "bridge",
						"Going to download orphan header from {} node: {:?}",
						P::SOURCE_NAME,
						parent_id,
					);

					source_orphan_header_future.set(source_client.header_by_hash(parent_id.1).fuse());
				} else if let Some(id) = sync.select_new_header_to_download() {
					log::debug!(
						target: "bridge",
						"Going to download new header from {} node: {:?}",
						P::SOURCE_NAME,
						id,
					);

					source_new_header_future.set(source_client.header_by_number(id).fuse());
				} else {
					source_maybe_client = Some(source_client);
				}
			}
		}
	});
}

/// Future that resolves into given value after given timeout.
async fn delay<T>(timeout: Duration, retval: T) -> T {
	async_std::task::sleep(timeout).await;
	retval
}

/// Stream that emits item every `timeout_ms` milliseconds.
fn interval(timeout: Duration) -> impl futures::Stream<Item = ()> {
	futures::stream::unfold((), move |_| async move {
		delay(timeout, ()).await;
		Some(((), ()))
	})
}

/// Process result of the future that may have been caused by connection failure.
fn process_future_result<TClient, TResult, TError, TGoOfflineFuture>(
	maybe_client: &mut Option<TClient>,
	client: TClient,
	result: Result<TResult, TError>,
	on_success: impl FnOnce(TResult),
	go_offline_future: &mut std::pin::Pin<&mut futures::future::Fuse<TGoOfflineFuture>>,
	go_offline: impl FnOnce(TClient) -> TGoOfflineFuture,
	error_pattern: impl FnOnce() -> String,
) where
	TError: std::fmt::Debug + MaybeConnectionError,
	TGoOfflineFuture: FutureExt,
{
	match result {
		Ok(result) => {
			*maybe_client = Some(client);
			on_success(result);
		}
		Err(error) => {
			if error.is_connection_error() {
				go_offline_future.set(go_offline(client).fuse());
			} else {
				*maybe_client = Some(client);
			}

			log::error!(target: "bridge", "{}: {:?}", error_pattern(), error);
		}
	}
}

/// Print synchronization progress.
fn print_sync_progress<P: HeadersSyncPipeline>(
	progress_context: (Instant, Option<P::Number>, Option<P::Number>),
	eth_sync: &crate::sync::HeadersSync<P>,
) -> (Instant, Option<P::Number>, Option<P::Number>) {
	let (prev_time, prev_best_header, prev_target_header) = progress_context;
	let now_time = Instant::now();
	let (now_best_header, now_target_header) = eth_sync.status();

	let need_update = now_time - prev_time > Duration::from_secs(10)
		|| match (prev_best_header, now_best_header) {
			(Some(prev_best_header), Some(now_best_header)) => {
				now_best_header.0.saturating_sub(prev_best_header) > 10.into()
			}
			_ => false,
		};
	if !need_update {
		return (prev_time, prev_best_header, prev_target_header);
	}

	log::info!(
		target: "bridge",
		"Synced {:?} of {:?} headers",
		now_best_header.map(|id| id.0),
		now_target_header,
	);
	(now_time, now_best_header.clone().map(|id| id.0), *now_target_header)
}
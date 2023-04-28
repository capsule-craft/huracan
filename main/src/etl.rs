use std::{
	fmt::{Display, Formatter},
	iter::zip,
};

use anyhow::Result;
use async_stream::stream;
use bson::doc;
use futures::Stream;
use futures_batch::ChunksTimeoutStreamExt;
use mongodb::Collection;
use sui_sdk::{
	apis::ReadApi,
	rpc_types::{
		ObjectChange as SuiObjectChange, SuiGetPastObjectRequest, SuiObjectData, SuiObjectDataOptions,
		SuiPastObjectResponse, SuiTransactionBlockResponseOptions, SuiTransactionBlockResponseQuery,
	},
};
use sui_types::{base_types::SequenceNumber, digests::TransactionDigest};

use crate::_prelude::*;

// sui allows a max of 50 objects to be queried for at once, at least on some endpoints
// (e.g. on `try_multi_get_parsed_past_object`)
const SUI_QUERY_MAX_RESULT_LIMIT: usize = 50;

#[derive(Clone, Debug, Serialize, Deserialize, PulsarMessage)]
pub struct ObjectSnapshot {
	pub digest: TransactionDigest,
	pub change: SuiObjectChange,
	pub object: Option<SuiObjectData>,
}

impl ObjectSnapshot {
	fn new(digest: TransactionDigest, change: SuiObjectChange) -> Self {
		Self { digest, change, object: None }
	}
}

impl ObjectSnapshot {
	fn get_change_version(&self) -> SequenceNumber {
		use SuiObjectChange::*;
		match &self.change {
			Published { version, .. }
			| Created { version, .. }
			| Mutated { version, .. }
			| Transferred { version, .. }
			| Deleted { version, .. }
			| Wrapped { version, .. } => *version,
		}
	}

	fn get_past_object_request(&self) -> SuiGetPastObjectRequest {
		SuiGetPastObjectRequest { object_id: self.change.object_id(), version: self.get_change_version() }
	}

	// TODO is this exactly what we want? skip fetching objects for anything but these 3 types of changes?
	fn skip_fetching_object(&self) -> bool {
		use SuiObjectChange::*;
		match &self.change {
			Published { .. } | Created { .. } | Mutated { .. } => false,
			_ => true,
		}
	}
}

pub async fn extract<'a, P: Fn(Option<TransactionDigest>, TransactionDigest) + 'a>(
	sui: &'a ReadApi,
	mut rx_term: tokio::sync::oneshot::Receiver<()>,
	start_from: Option<TransactionDigest>,
	on_next_page: P,
) -> Result<impl Stream<Item = ObjectSnapshot> + 'a> {
	let q = SuiTransactionBlockResponseQuery::new(
		None,
		Some(SuiTransactionBlockResponseOptions::new().with_object_changes()),
	);
	let mut cursor = start_from;
	let mut skip_page = false;
	let mut retry_count = 0;

	Ok(stream! {
		loop {
			tokio::select! {
				page = sui.query_transaction_blocks(q.clone(), cursor, Some(SUI_QUERY_MAX_RESULT_LIMIT), false) => {
					match page {
						Ok(page) => {
							retry_count = 0;
							if !skip_page {
								for tx_block in page.data {
									if let Some(changes) = tx_block.object_changes {
										for change in changes {
											yield ObjectSnapshot::new(tx_block.digest.clone(), change);
										}
									}
								}
							}
							if page.next_cursor.is_none() {
								info!("no next page info from sui, will sleep for 10s, then try to find the next page...");
								skip_page = true;
								tokio::time::sleep(Duration::from_millis(10_000)).await;
							} else {
								skip_page = false;
								on_next_page(cursor.clone(), page.next_cursor.unwrap());
								cursor = page.next_cursor;
							}
						},
						Err(err) => {
							warn!(error = ?err, "There was an error reading object changes... retrying (retry #{}) after short timeout", retry_count);
							retry_count += 1;
							tokio::time::sleep(Duration::from_millis(500)).await;
						}
					}
				}
				_ = &mut rx_term => break
			}
		}
	})
}

pub enum StepStatus {
	Ok,
	Err,
}

impl Display for StepStatus {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		match self {
			Self::Ok => f.write_str("Ok"),
			Self::Err => f.write_str("Err"),
		}
	}
}

pub async fn transform<'a, S: Stream<Item = ObjectSnapshot> + 'a>(
	stream: S,
	sui: &'a ReadApi,
) -> impl Stream<Item = (StepStatus, ObjectSnapshot)> + 'a {
	// batch incoming items so we can amortize the cost of sui api calls,
	// but send them off one by one, so any downstream consumer (e.g. Pulsar client) can apply their
	// own batching logic, if necessary (e.g. Pulsar producer will auto-batch transparently, if configured)

	let stream = stream.chunks_timeout(SUI_QUERY_MAX_RESULT_LIMIT, Duration::from_millis(1_000));

	let query_opts = SuiObjectDataOptions {
		show_type:                 true,
		show_owner:                true,
		show_previous_transaction: true,
		show_display:              false,
		show_content:              true,
		show_bcs:                  true,
		show_storage_rebate:       true,
	};

	fn parse_past_object_response(res: SuiPastObjectResponse) -> Option<SuiObjectData> {
		use SuiPastObjectResponse::*;
		match res {
			VersionFound(obj) => return Some(obj),
			ObjectDeleted(o) => {
				// TODO this can't be right (at least the message needs fixing, but I suspect more than that)
				info!(object_id = ?o.object_id, version = ?o.version, digest = ?o.digest, "object is in some further object change, skipping for now");
			}
			ObjectNotExists(object_id) => {
				info!(object_id = ?object_id, "object doesn't exist");
			}
			VersionNotFound(object_id, version) => {
				info!(object_id = ?object_id, version = ?version, "object not found");
			}
			VersionTooHigh { object_id, asked_version, latest_version } => {
				info!(object_id = ?object_id, asked_version = ?asked_version, latest_version = ?latest_version, "object version too high");
			}
		};
		None
	}

	stream! {
		for await mut chunk in stream {
			// filter and remove changes that we shouldn't fetch objects for, and stream them as is
			let skip = chunk.drain_filter(|o| o.skip_fetching_object()).collect::<Vec<_>>();
			for item in skip {
				yield (StepStatus::Ok, item);
			}
			let query_objs = chunk.iter().map(|o| o.get_past_object_request()).collect::<Vec<_>>();
			match sui.try_multi_get_parsed_past_object(query_objs, query_opts.clone()).await {
				Err(err) => {
					warn!(error = format!("{err:?}"), "cannot fetch object data for one or more objects, retrying them individually");
					// try one by one
					// TODO this should be super easy to do in parallel, firing off the reqs on some tokio thread pool executor
					for mut snapshot in chunk {
						match sui.try_get_parsed_past_object(snapshot.change.object_id(), snapshot.get_change_version(), query_opts.clone()).await {
							Err(err) => {
								// TODO add info about object to log
								error!(error = format!("{err:?}"), "individual fetch also failed");
								yield (StepStatus::Err, snapshot);
							},
							Ok(res) => {
								if let Some(obj) = parse_past_object_response(res) {
									snapshot.object = Some(obj);
									yield (StepStatus::Ok, snapshot);
								}
							}
						}
					}
				},
				Ok(objs) => {
					// XXX: relying on a possible Sui API implementation detail
					// the sui endpoint is implemented such that the response items are in the same
					// order as the input items, so we don't have to search or otherwise match them
					if objs.len() != chunk.len() {
						panic!("sui.try_multi_get_parsed_past_object() mismatch between input and result len!");
					}
					for (mut snapshot, res) in zip(chunk, objs) {
						// TODO if we can't get object info, do we really want to skip indexing this change? or is there something more productive we can do?
						if let Some(obj) = parse_past_object_response(res) {
							snapshot.object = Some(obj);
							yield (StepStatus::Ok, snapshot);
						}
					}
				}
			}
		}
	}
}

pub async fn load<S: Stream<Item = ObjectSnapshot>>(
	stream: S,
	collection: &'a Collection<ObjectSnapshot>,
) -> Result<impl Stream<Item = (StepStatus, ObjectSnapshot)>> {
	let stream = stream.chunks_timeout(64, Duration::from_millis(1_000));

	Ok(stream! {
		for await chunk in stream {
			// TODO batching is only planned, not implemented yet
			// for now mongo's rust driver doesn't offer a way to do proper bulk querying / batching
			// there's only an API for inserting many, but not for updating or deleting many, and
			// neither an API that lets us do all of those within a single call
			// so in order to work around that, we do the following:
			// group items by the type of query they need to execute, and run each of those groups in one call each
			// we also have to provide our own bulk update + delete methods, based on the db.run_command API
			for item in chunk {
				match item.change {
					SuiObjectChange::Deleted { object_id, version, .. } => {
						info!(object_id = ?object_id, version = ?version, "deleting object");
						if let Result::Err(err) = collection.delete_one(doc! { "_id": object_id.to_string(), "version": version.to_string() }, None).await {
							error!(object_id = ?object_id, version = ?version, "failed to delete: {}", err);
							yield (StepStatus::Err, item);
						} else {
							yield (StepStatus::Ok, item);
						}
					}
					SuiObjectChange::Created { object_id, version, .. } | SuiObjectChange::Mutated { object_id, version, .. } => {
						info!(object_id = ?object_id, version = ?version, "inserting object");
						let filter = doc! { "_id": object_id.to_string(), "version": version.to_string() };
						let update_options = UpdateOptions::builder().upsert(true).build();

						let res = collection
								.update_one(
									filter,
									doc! {
										"$set": {
											"_id": object_id.to_string(),
											"version": version.to_string(),
											"object": bson::to_bson(item.object.as_ref().unwrap()).unwrap().as_document().unwrap(),
										}
									},
									update_options,
								)
								.await;
						if let Result::Err(err) = res {
							error!(object_id = ?object_id, version = ?version, "failed to upsert: {}", err);
							yield (StepStatus::Err, item);
						} else {
							yield (StepStatus::Ok, item);
						}
					}
					_ => {}
				}
			}
		}
	})
}
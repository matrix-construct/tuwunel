//! Synchronize this server with primary by requesting stream of WAL frames.

use futures::StreamExt;
use reqwest::StatusCode;
use tuwunel_core::{Err, Result, err, implement, info, warn};
use tuwunel_database::{FrameKind, WalFrame};
use url::Url;

/// Stream WAL frames from the primary until disconnect, promotion, error
/// or server shutdown.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(url = primary_url.as_str()),
	err,
)]
pub(super) async fn sync(&self, primary_url: &Url) -> Result {
	let resume_seq = self.get_resume_seq()?.to_string();
	let mut url = primary_url.join("_tuwunel/cluster/sync")?;

	url.query_pairs_mut()
		.append_pair("since", &resume_seq)
		.finish();

	let resp = self
		.authed_get(&url)
		.await
		.map_err(|e| err!(Database("GET {url}: {e}")))?;

	let status = resp.status();
	if status == StatusCode::GONE {
		return Err!(Database("WAL gap: 410 Gone from primary"));
	}
	if !status.is_success() {
		return Err!(Database("Primary returned {status} for WAL stream"));
	}

	info!("WAL stream connected; starting from seq {resume_seq}");

	let mut byte_stream = resp.bytes_stream();
	let mut buf: Vec<u8> = Vec::new();
	while self.server.is_running() && !self.is_promoted() {
		tokio::select! {
			() = self.server.until_shutdown() => return Ok(()),
			() = self.promote_notify.notified() => return Ok(()),
			chunk = byte_stream.next() => match chunk {
				None => return Err!(Database("Primary closed WAL stream")),
				Some(Err(e)) => return Err!(Database("WAL stream read: {e}")),
				Some(Ok(chunk)) => {
					buf.extend_from_slice(&chunk);
					self.drain_frames(&mut buf)?;
				},
			},
		}
	}

	Ok(())
}

/// Parse and apply as many complete frames as possible from `buf`.
#[implement(super::Service)]
fn drain_frames(&self, buf: &mut Vec<u8>) -> Result {
	let mut offset = 0;
	while let Ok((frame, remain)) = WalFrame::decode(&buf[offset..]) {
		offset = buf.len().saturating_sub(remain.len());
		self.apply_frame(&frame)?;
	}

	buf.drain(..offset);

	Ok(())
}

/// Apply a single frame to the local database.
#[implement(super::Service)]
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(?frame),
	err,
)]
fn apply_frame(&self, frame: &WalFrame) -> Result {
	let next = frame.next_resume_seq();

	match frame.kind() {
		| FrameKind::Data => {
			assert!(frame.count > 0, "Count expected on data frame.");
			assert!(next > 0, "Non-zero sequence expected after data frame.");

			if !frame.batch_data.is_empty() {
				self.db
					.engine
					.write_raw_batch(&frame.batch_data)?;
			}

			self.set_resume_seq(next)?;
		},
		| _ => {
			assert_eq!(frame.count, 0, "Count only expected on data frame.");

			let cur = self.get_resume_seq()?;
			if cur < next {
				warn!(?cur, ?next, "Sequence number possibly desynchronized...");
			}
		},
	}

	Ok(())
}

/// Send an authenticated GET request to the primary.
#[implement(super::Service)]
#[tracing::instrument(
	name = "get",
	level = "debug",
	skip_all,
	fields(url = url.as_str()),
	err,
)]
async fn authed_get(&self, url: &Url) -> Result<reqwest::Response> {
	let mut req = self.client.get(url.clone());
	if let Some(ref token) = self.server.config.rocksdb_replication_token {
		req = req.header("x-tuwunel-replication-token", token.as_str());
	}

	req.send().await.map_err(Into::into)
}

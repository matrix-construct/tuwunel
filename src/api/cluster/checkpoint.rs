use std::path::{Path, PathBuf};

use axum::{
	body::Body,
	extract::State,
	http::{StatusCode, header},
	response::{IntoResponse, Response},
};
use bytes::Bytes;
use tuwunel_core::{Err, Result, err, utils::time::now};

struct TempDir(PathBuf);

/// `GET /_tuwunel/cluster/checkpoint`
///
/// Creates a RocksDB checkpoint of the primary's database and streams it as a
/// tar archive. The `X-Tuwunel-Checkpoint-Sequence` response header carries
/// the WAL sequence number at checkpoint creation time; the secondary uses
/// this as its initial `?since=` value when it begins WAL streaming.
///
/// The caller is responsible for pausing WAL consumption while restoring the
/// checkpoint and then resuming from `X-Tuwunel-Checkpoint-Sequence`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_checkpoint(State(services): State<crate::State>) -> impl IntoResponse {
	let db = services.db.clone();

	// Build the checkpoint and tar it in a blocking thread.
	let result = services
		.server
		.runtime()
		.spawn_blocking(move || -> Result<(Bytes, u64)> {
			let tmp = tempfile_checkpoint_dir()?;
			let checkpoint_path = tmp.path().join("checkpoint");
			let seq = db.engine.create_checkpoint(&checkpoint_path)?;

			// Build tar archive in memory.
			let mut archive_bytes: Vec<u8> = Vec::new();
			{
				let mut builder = tar::Builder::new(&mut archive_bytes);

				builder
					.append_dir_all("checkpoint", &checkpoint_path)
					.map_err(|e| err!("{e}"))?;

				builder.finish().map_err(|e| err!("{e}"))?;
			};

			Ok((Bytes::from(archive_bytes), seq))
		})
		.await;

	match result {
		| Ok(Ok((bytes, seq))) => Ok(Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/x-tar")
			.header("x-tuwunel-checkpoint-sequence", seq.to_string())
			.body(Body::from(bytes))
			.expect("Failed to build checkpoint response")),

		| Ok(Err(e)) => Err!(HttpJson(INTERNAL_SERVER_ERROR, {
			"error": format!("Checkpoint creation failed: {e}")
		})),

		| Err(e) => Err!(HttpJson(INTERNAL_SERVER_ERROR, {
			"error": format!("Spawn_blocking panicked: {e}")
		})),
	}
}

/// Creates a temporary directory that is automatically removed on drop.
///
/// We use a simple wrapper around `std::fs::create_dir_all` on a
/// `tempfile::TempDir` equivalent so we don't add a `tempfile` dependency.
/// Instead, we create a uniquely-named subdirectory in the OS temp dir and
/// delete it ourselves.
fn tempfile_checkpoint_dir() -> Result<TempDir> {
	use std::{env::temp_dir, process};

	let ts = now().as_nanos();
	let dir = temp_dir().join(format!("tuwunel-checkpoint-{ts}-{}", process::id()));

	std::fs::create_dir_all(&dir).map_err(|e| err!("{e}"))?;

	Ok(TempDir(dir))
}

impl TempDir {
	fn path(&self) -> &Path { &self.0 }
}

impl Drop for TempDir {
	fn drop(&mut self) { std::fs::remove_dir_all(&self.0).ok(); }
}

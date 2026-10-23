//! Routes thumbnail requests through stored and generated media.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use futures::{StreamExt, pin_mut};
use ruma::{Mxc, UserId, http_headers::ContentDisposition};
use tokio::{sync::Notify, time::timeout};
use tuwunel_core::{
	Err, Result, async_noinline, err, implement,
	utils::{result::LogDebugErr, stream::IterStream},
};

#[cfg(feature = "media_thumbnail")]
use super::generate::picture_dim;
#[cfg(all(test, feature = "media_thumbnail"))]
use super::tests::source_fetched;
use super::{
	super::{Media, data::Metadata},
	Animate, Dim,
};

impl super::super::Service {
	/// Uploads or replaces a file thumbnail.
	///
	/// Metadata is written first, then the supplied bytes replace the stored
	/// media body for the requested dimensions.
	#[tracing::instrument(
		level = "debug",
		ret(level = "debug")
		skip(self, file),
	)]
	pub async fn upload_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
		dim: &Dim,
		file: &[u8],
	) -> Result {
		let key =
			self.db
				.create_file_metadata(mxc, None, dim, content_disposition, content_type)?;

		// TODO: Remove dangling metadata when file creation fails.
		self.create_media_file(&key, file).await?;

		Ok(())
	}

	/// Answers a thumbnail request, fetching from the peer when it is remote.
	///
	/// The dimension a fetch asks the peer for is the dimension the answer is
	/// filed under, and every later lookup seeks the normalized one, so the
	/// three have to agree or nothing found on one request is found on the
	/// next. Past every bucket there is no dimension to ask at, and the request
	/// names the original file instead.
	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_or_fetch_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
		animate: Animate,
		timeout_ms: Duration,
		user: &UserId,
	) -> Result<Media> {
		if let Ok(media) = self
			.get_thumbnail(mxc, dim, animate, Some(timeout_ms))
			.await
		{
			return Ok(media);
		}

		if self
			.services
			.globals
			.server_is_ours(mxc.server_name)
		{
			return Err!(Request(NotFound("Local thumbnail not found.")));
		}

		let lock = self.federation_mutex.lock(&mxc.to_string()).await;
		let normalized = dim.normalized();

		if self
			.db
			.file_metadata_exists(mxc, &normalized)
			.await
		{
			drop(lock);
			return self.get_thumbnail(mxc, dim, animate, None).await;
		}

		let fetched = match normalized.is_original() {
			| true =>
				self.fetch_remote_content(mxc, None, timeout_ms)
					.await?,
			| false =>
				self.fetch_remote_thumbnail(mxc, None, timeout_ms, &normalized, animate)
					.await?,
		};

		// a peer may ignore the parameter and this answers the caller directly,
		// so the variant is repaired before it leaves rather than one request on
		if animate.accepts_fetched(&fetched) {
			return Ok(fetched.media);
		}

		self.store_still(mxc, &normalized, fetched.media)
			.await
	}

	/// Downloads a thumbnail, waiting for a pending upload when requested.
	///
	/// The supplied duration bounds that wait. The future is boxed because the
	/// still-repair path pulls the thumbnailer
	/// into it, and inlining that into every caller overflows the layout depth
	/// limit in the federation handler.
	#[async_noinline]
	#[tracing::instrument(
		level = "debug",
		err(level = "debug")
		skip(self),
	)]
	pub async fn get_thumbnail<'a>(
		&'a self,
		mxc: &'a Mxc<'_>,
		dim: &'a Dim,
		animate: Animate,
		timeout_duration: Option<Duration>,
	) -> Result<Media> {
		if let Ok(media) = self.get_stored_thumbnail(mxc, dim, animate).await {
			return Ok(media);
		}

		let Some(timeout_duration) = timeout_duration else {
			return Err!(Request(NotFound("Media thumbnail not found.")));
		};

		let Ok(_pending) = self.db.search_pending_mxc(mxc).await else {
			return Err!(Request(NotFound("Media thumbnail not found.")));
		};

		let notifier = self
			.mxc_state
			.notifiers
			.lock()?
			.entry(mxc.to_string().into())
			.or_insert_with(|| Arc::new(Notify::new()))
			.clone();

		if timeout(timeout_duration, notifier.notified())
			.await
			.is_err()
		{
			return Err!(Request(NotYetUploaded("Media has not been uploaded yet.")));
		}

		self.get_stored_thumbnail(mxc, dim, animate).await
	}

	/// Downloads a stored or generated thumbnail.
	///
	/// Requests normalize to a bounded storage bucket. An existing variant is
	/// returned directly; a missing variant is generated from the original or
	/// answered from promoted storage when the original row is absent.
	#[tracing::instrument(
		name = "thumbnail",
		level = "debug",
		err(level = "trace")
		skip(self),
	)]
	pub async fn get_stored_thumbnail(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
		animate: Animate,
	) -> Result<Media> {
		let dim = dim.normalized();

		// the sentinel is the key the original is stored under rather than a
		// size, so a request reaching it is answered from the original itself
		if dim.is_original() {
			return self.answer_original(mxc, animate).await;
		}

		if let Ok(metadata) = self
			.db
			.search_file_metadata(mxc, &dim, animate)
			.await
		{
			return self
				.answer_stored(mxc, &dim, animate, metadata)
				.await;
		}

		let Ok(metadata) = self.original_metadata(mxc).await else {
			return self.answer_promoted(mxc, animate).await;
		};

		self.get_thumbnail_generate(mxc, &dim, animate, metadata)
			.await
	}
}

/// Answers a request past every bucket, which names the original file.
///
/// The original's own row is keyed at the sentinel, where there is no size to
/// re-encode at, so a picture the request will not accept stands in at its own
/// dimensions. Those are the largest a still may carry without upscaling, so
/// the stand-in covers any request the original itself covers and is the best
/// available where it does not.
///
/// The future is not inlined: this branch reaches the thumbnailer, and folding
/// that into every caller overflows the layout depth limit.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
#[async_noinline]
#[tracing::instrument(name = "original", level = "debug", skip(self))]
async fn answer_original<'a>(&'a self, mxc: &'a Mxc<'_>, animate: Animate) -> Result<Media> {
	let Ok(data) = self.original_metadata(mxc).await else {
		return self.answer_promoted(mxc, animate).await;
	};

	let bytes = self.fetch_bytes(&data.key).await?;

	if animate.accepts_picture(&bytes) {
		return Ok(into_media(data, bytes.into()));
	}

	let dim = picture_dim(&bytes);

	if let Ok(metadata) = self
		.db
		.search_file_metadata(mxc, &dim, animate)
		.await
	{
		drop((bytes, data));

		return self
			.answer_stored(mxc, &dim, animate, metadata)
			.await;
	}

	// no still can be derived from a picture that will not decode, and every
	// bucket answers that with the original rather than refusing
	let Ok(image) = self.decode(&bytes) else {
		return match animate.accepts_fallback(&bytes) {
			| true => Ok(into_media(data, bytes.into())),
			| false => Err!(Request(NotFound("Media thumbnail not found."))),
		};
	};

	drop((bytes, data));

	self.store_thumbnail(mxc, &dim, image).await
}

/// Hands the original back, there being no thumbnailer to stand a still in.
///
/// A build without the feature keeps serving what it holds rather than
/// refusing, so a request that forbade animation goes unhonored here.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
#[tracing::instrument(name = "original", level = "debug", skip_all)]
async fn answer_original(&self, mxc: &Mxc<'_>, animate: Animate) -> Result<Media> {
	let Ok(data) = self.original_metadata(mxc).await else {
		return self.answer_promoted(mxc, animate).await;
	};

	self.get_thumbnail_saved(data).await
}

/// Metadata for the original file, which every thumbnail is derived from.
///
/// Its row is keyed at the sentinel dimension, and nothing is withheld from
/// this lookup: the row is the original rather than a variant of it.
#[implement(super::super::Service)]
pub(super) async fn original_metadata(&self, mxc: &Mxc<'_>) -> Result<Metadata> {
	self.db
		.search_file_metadata(mxc, &Dim::default(), Animate::Allowed)
		.await
}

/// Answers from stored media when no metadata row names the original.
///
/// The original may be lazy preview media promoted on this very request, which
/// leaves no row behind, and only a picture is worth serving in a thumbnail's
/// place. No row having named it, the request's own preference is the only gate
/// it passes, so the picture is read here as it is anywhere else.
#[implement(super::super::Service)]
#[tracing::instrument(name = "promoted", level = "debug", skip(self))]
async fn answer_promoted(&self, mxc: &Mxc<'_>, animate: Animate) -> Result<Media> {
	let media = self.get_stored(mxc).await?;
	let servable = media
		.content_type
		.as_deref()
		.is_some_and(|content_type| content_type.starts_with("image/"))
		&& animate.accepts_picture(&media.content);

	servable
		.then_some(media)
		.ok_or_else(|| err!(Request(NotFound("Media not found."))))
}

/// The stored bytes for a thumbnail row, from the first provider holding them.
///
/// Returning the shared `Bytes` spares a caller that only reads the picture
/// the `Vec` copy `Media` would force on it.
#[implement(super::super::Service)]
#[tracing::instrument(name = "fetch", level = "debug", skip_all)]
pub(super) async fn fetch_bytes(&self, key: &[u8]) -> Result<Bytes> {
	#[cfg(all(test, feature = "media_thumbnail"))]
	source_fetched();

	let path = self.get_media_name_sha256(key);
	let fetch = self
		.storage_providers()
		.stream()
		.filter_map(async |provider| {
			provider
				.get(path.as_str())
				.await
				.log_debug_err()
				.ok()
		});

	pin_mut!(fetch);

	fetch
		.next()
		.await
		.ok_or_else(|| err!(Request(NotFound("Media not found."))))
}
/// Answers a request from a stored row, re-deriving a still if it animates.
///
/// The type a row is stored under is whatever produced it claimed, and a peer
/// is free to claim wrongly, so the picture itself decides once it is in hand.
/// A row that may not answer is re-encoded and the still left behind for the
/// next request.
#[cfg(feature = "media_thumbnail")]
#[implement(super::super::Service)]
#[tracing::instrument(name = "stored", level = "debug", skip(self, data))]
async fn answer_stored(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	animate: Animate,
	data: Metadata,
) -> Result<Media> {
	let bytes = self.fetch_bytes(&data.key).await?;

	if animate.accepts_picture(&bytes) {
		return Ok(into_media(data, bytes.into()));
	}

	let image = self.decode_still(&bytes)?;

	drop((bytes, data));

	self.store_thumbnail(mxc, dim, image).await
}

/// Hands the stored row back whatever its picture holds.
///
/// Judging it would only be worth doing if a still could then be derived, and
/// a build without the feature has no thumbnailer to derive one with.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
#[tracing::instrument(name = "stored", level = "debug", skip_all)]
async fn answer_stored(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	_animate: Animate,
	data: Metadata,
) -> Result<Media> {
	self.get_thumbnail_saved(data).await
}

/// Hands the picture back as it stands, there being no thumbnailer.
///
/// A build without the feature keeps serving what it holds rather than
/// refusing, so a request for a still goes unhonored instead of failing.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
#[tracing::instrument(name = "still", level = "debug", skip_all)]
pub(in super::super) async fn store_still(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	animated: Media,
) -> Result<Media> {
	Ok(animated)
}

/// Hands the original back in place of the thumbnail it cannot generate.
///
/// The row this is given is the original's own, so the caller is answered with
/// media the server holds rather than being refused outright.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
#[tracing::instrument(name = "fallback", level = "debug", skip_all)]
pub(super) async fn get_thumbnail_generate(
	&self,
	_mxc: &Mxc<'_>,
	_dim: &Dim,
	_animate: Animate,
	data: Metadata,
) -> Result<Media> {
	self.get_thumbnail_saved(data).await
}

/// Answers a thumbnail row from the picture already in storage.
///
/// The response hands its body over by value, so the bytes are converted
/// rather than read in place; the conversion reclaims the storage buffer
/// whenever this holds the only handle to it.
#[cfg(not(feature = "media_thumbnail"))]
#[implement(super::super::Service)]
#[tracing::instrument(name = "saved", level = "debug", skip_all)]
pub(super) async fn get_thumbnail_saved(&self, data: Metadata) -> Result<Media> {
	let bytes = self.fetch_bytes(&data.key).await?;

	Ok(into_media(data, bytes.into()))
}
/// Transfers stored metadata and content into a media response.
///
/// Content type and disposition move out of the metadata row without cloning.
pub(super) fn into_media(data: Metadata, content: Vec<u8>) -> Media {
	Media {
		content,
		content_type: data.content_type,
		content_disposition: data.content_disposition,
	}
}

//! Common interface over local and S3-compatible object stores.
//!
//! Providers apply an optional base path, choose single-part or multipart
//! uploads, and expose streaming reads and deletes. Backend failures are
//! translated into the service's shared error type.

/// Local-filesystem storage-provider construction.
///
/// The constructor validates the configured directory and can create it when
/// requested before wrapping it as a provider.
/// Disabled local configurations return no provider.
pub mod local;

/// S3-compatible storage-provider construction.
///
/// The constructor applies endpoint, credential, transport, and signing
/// options before wrapping the object-store client as a provider.
/// Configurations with neither a URL nor a bucket return no provider.
pub mod s3;

#[cfg(test)]
mod tests;

use std::{
	iter::{from_fn, once},
	ops::Range,
	sync::Arc,
	time::Duration,
};

use bytes::Bytes;
use derive_more::Debug;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use http::Method;
use object_store::{
	Attributes, CopyMode, DynObjectStore, GetResult, MultipartUpload, ObjectMeta, ObjectStore,
	ObjectStoreExt, PutPayload, PutResult, path::Path, signer::Signer,
};
use tuwunel_core::{
	Error, Result,
	config::StorageProvider,
	debug, err, error,
	error::error_chain,
	extract_variant, implement, info, trace,
	utils::{
		BoolExt,
		result::FlatOk,
		stream::{IterStream, TryReadyExt},
	},
};
use url::Url;

/// One configured object-storage backend.
///
/// The provider normalizes configured paths and transfer policies before
/// delegating operations to its local or S3-compatible object store.
/// Optional startup checks and URL signing remain backend capabilities.
#[derive(Debug)]
pub struct Provider {
	/// Configuration identifier for this provider.
	pub name: String,

	/// Backend-specific configuration used to construct this provider.
	pub config: StorageProvider,

	/// Erased object-store implementation receiving provider operations.
	pub(crate) provider: Box<DynObjectStore>,

	#[debug(skip)]
	/// Optional backend signer used to create time-limited object URLs.
	pub(crate) signer: Option<Arc<dyn Signer>>,

	/// Prefix prepended to logical object paths before backend operations.
	pub(crate) base_path: Option<Path>,

	startup_check: bool,

	#[expect(unused)]
	#[debug(skip)]
	services: Arc<crate::services::OnceServices>,
}

/// One streamed object chunk with its returned range and complete object size.
///
/// Every chunk from one fetch carries a clone of the range and size metadata
/// reported by the backend for that request.
/// Stream and backend failures are represented separately as error items.
pub type FetchItem = (Bytes, (Range<u64>, u64));

/// One streamed object chunk with shared response metadata and attributes.
///
/// The metadata tuple is shared by [`Arc`] across every chunk from the same
/// fetch, avoiding a per-chunk clone of the backend response details.
/// Stream and backend failures are represented separately as error items.
pub type FetchMetaItem = (Bytes, Arc<(Range<u64>, ObjectMeta, Attributes)>);

/// Starts this provider and performs its configured connectivity check.
///
/// Providers with startup checks disabled become ready without backend I/O.
/// An enabled check lists at most one object and propagates any backend error.
#[implement(Provider)]
#[tracing::instrument(skip_all, err)]
pub(super) async fn start(self: &Arc<Self>) -> Result {
	if self.startup_check {
		self.startup_check().await?;
	}

	Ok(())
}

#[implement(Provider)]
#[tracing::instrument(name = "check", skip_all, err)]
async fn startup_check(self: &Arc<Self>) -> Result {
	debug!(
		name = ?self.name,
		"Checking storage provider client connection...",
	);
	self.ping()
		.inspect_ok(|()| {
			info!(
				name = %self.name,
				"Connected to storage provider"
			);
		})
		.await
}

/// Stores a streamed object under `path`.
///
/// Supplying the total size permits a single-part upload below the configured
/// threshold. A missing or large size selects multipart upload instead.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
		?size,
	)
)]
pub async fn put<S, T>(&self, path: &str, size: Option<usize>, input: S) -> Result<PutResult>
where
	S: Stream<Item = Result<T>> + Send,
	PutPayload: From<T> + From<PutPayload>,
{
	if size.is_none_or(|size| size >= self.multipart_threshold()) {
		return self.put_multi(path, input).await;
	}

	debug!(
		?size,
		threshold = ?self.multipart_threshold(),
		"Selecting single-part upload..."
	);

	let payload: PutPayload = input
		.map_ok(PutPayload::from)
		.try_collect::<Vec<_>>()
		.await?
		.into_iter()
		.map(Bytes::from)
		.collect();

	self.put_single(path, payload).await
}

/// Stores one contiguous object under `path`.
///
/// The input length selects single-part or multipart upload against the
/// configured threshold. Backend upload failures are propagated.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub async fn put_one<T>(&self, path: &str, input: T) -> Result<PutResult>
where
	PutPayload: From<T> + From<PutPayload>,
{
	let payload: PutPayload = input.into();

	if payload.content_length() < self.multipart_threshold() {
		return self.put_single(path, payload).await;
	}

	let part_size = self.multipart_part_size();

	debug!(
		len = ?payload.content_length(),
		threshold = ?self.multipart_threshold(),
		?part_size,
		"Selecting multi-part upload..."
	);

	self.put_multi(path, chunked(payload, part_size).try_stream())
		.await
}

/// Stores streamed input through a multipart upload.
///
/// Input chunks are written as ordered multipart parts. Upload cleanup and
/// backend failures are delegated to the object-store implementation.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
async fn put_multi<S, T>(&self, path: &str, input: S) -> Result<PutResult>
where
	S: Stream<Item = Result<T>> + Send,
	PutPayload: From<T> + From<PutPayload>,
{
	let path = self.to_abs_path(path)?;
	let mut handle = self
		.provider
		.put_multipart(&path)
		.map_err(Error::from)
		.await?;

	match input
		.try_for_each(|t| handle.put_part(t.into()).map_err(Error::from))
		.inspect_err(|e| error!(?path, chain = %error_chain(e), "Failed to store object"))
		.await
	{
		| Ok(()) =>
			handle
				.complete()
				.map_err(Error::from)
				.inspect_err(|e| {
					error!(
						?path,
						chain = %error_chain(e),
						"Failed to store object during completion",
					);
				})
				.await,

		| Err(e) =>
			handle
				.abort()
				.map_ok(|()| Err(e))
				.map_err(Error::from)
				.inspect_err(|e| {
					error!(
						?path,
						chain = %error_chain(e),
						"Additional errors during error handling",
					);
				})
				.await?,
	}
}

/// Stores contiguous input through a single-part upload.
///
/// The provider prefix is applied before the backend request. Backend failures
/// are propagated without retrying as multipart upload.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
async fn put_single(&self, path: &str, input: PutPayload) -> Result<PutResult> {
	let path = self.to_abs_path(path)?;

	self.provider
		.put(&path, input)
		.map_err(Error::from)
		.await
}

/// Streams an object's bytes together with shared response metadata.
///
/// The provider prefix is applied to `path`, and each successful chunk shares
/// the same range, object metadata, and attributes. Load and stream failures
/// are returned as items rather than being discarded.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub fn fetch_with_metadata(
	&self,
	path: &str,
) -> impl Stream<Item = Result<FetchMetaItem>> + Send {
	self.load(path)
		.map_ok(|result| {
			let meta = (result.range.clone(), result.meta.clone(), result.attributes.clone());
			let data = Arc::new(meta);

			result
				.into_stream()
				.map_err(Error::from)
				.map_ok(move |bytes| (bytes, data.clone()))
		})
		.map_err(Error::from)
		.try_flatten_stream()
}

/// Streams an object's bytes together with its returned range and total size.
///
/// The provider prefix is applied to `path`. Load and stream failures are
/// returned as items, allowing callers to consume the body without buffering
/// the complete object.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub fn fetch(&self, path: &str) -> impl Stream<Item = Result<FetchItem>> + Send {
	self.load(path)
		.map_ok(|result| {
			let size = result.meta.size;
			let range = result.range.clone();

			result
				.into_stream()
				.map_err(Error::from)
				.map_ok(move |bytes| (bytes, (range.clone(), size)))
		})
		.map_err(Error::from)
		.try_flatten_stream()
}

/// Loads an entire object into one contiguous byte buffer.
///
/// The provider prefix is applied before the backend request. Backend and body
/// streaming failures are propagated to the caller.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub async fn get(&self, path: &str) -> Result<Bytes> {
	self.load(path)
		.map_ok(GetResult::bytes)
		.await?
		.map_err(Error::from)
		.await
}

/// Opens an object and returns the backend's raw read result.
///
/// The provider prefix is applied before the request. Callers can inspect the
/// returned range and metadata or consume its body as a stream.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub async fn load(&self, path: &str) -> Result<GetResult> {
	let path = self.to_abs_path(path)?;

	self.provider
		.get(&path)
		.map_err(Error::from)
		.await
}

/// Creates a time-limited GET URL when the backend supports signing.
///
/// The provider prefix is applied before signing. Backends without a signer,
/// such as local filesystem providers, return `None` without performing I/O.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
		?ttl,
	)
)]
pub async fn signed_get_url(&self, path: &str, ttl: Duration) -> Result<Option<Url>> {
	let Some(signer) = self.signer.as_ref() else {
		return Ok(None);
	};

	let path = self.to_abs_path(path)?;

	signer
		.signed_url(Method::GET, &path, ttl)
		.map_err(Error::from)
		.map_ok(Some)
		.await
}

/// Deletes one object from this provider.
///
/// This consumes [`Self::delete`] to completion and discards its yielded path.
/// Invalid paths and backend failures are propagated.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub async fn delete_one(self: &Arc<Self>, path: &str) -> Result {
	self.delete(once(path.to_owned()).stream())
		.map_ok(|_| ())
		.try_collect()
		.await
}

/// Lazily deletes each supplied object path.
///
/// The provider prefix is applied to every path before it reaches the backend.
/// Invalid paths and backend failures are emitted by the returned stream.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		provider = %self.name,
	)
)]
pub fn delete<S>(self: &Arc<Self>, paths: S) -> impl Stream<Item = Result<Path>> + Send
where
	S: Stream<Item = String> + Send + 'static,
{
	let this = self.clone();
	let paths = paths
		.map(Ok)
		.ready_and_then(move |path| {
			use object_store::{Error, path};

			this.to_abs_path(&path)
				.map_err(|_| Error::InvalidPath {
					source: path::Error::InvalidPath { path: path.into() },
				})
		})
		.boxed();

	self.provider
		.delete_stream(paths)
		.map_err(Error::from)
}

/// Renames an object within this provider.
///
/// Both paths receive the provider prefix. [`CopyMode::Create`] refuses an
/// existing destination, while [`CopyMode::Overwrite`] permits replacement.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?src,
		?dst,
		?overwrite,
	)
)]
pub async fn rename(&self, src: &str, dst: &str, overwrite: CopyMode) -> Result {
	let src = self.to_abs_path(src)?;
	let dst = self.to_abs_path(dst)?;

	match overwrite {
		| CopyMode::Overwrite => self.provider.rename(&src, &dst).left_future(),
		| CopyMode::Create => self
			.provider
			.rename_if_not_exists(&src, &dst)
			.right_future(),
	}
	.map_err(Error::from)
	.await
}

/// Copies an object within this provider.
///
/// Both paths receive the provider prefix. [`CopyMode::Create`] refuses an
/// existing destination, while [`CopyMode::Overwrite`] permits replacement.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?src,
		?dst,
		?overwrite,
	)
)]
pub async fn copy(&self, src: &str, dst: &str, overwrite: CopyMode) -> Result {
	let src = self.to_abs_path(src)?;
	let dst = self.to_abs_path(dst)?;

	match overwrite {
		| CopyMode::Overwrite => self.provider.copy(&src, &dst).left_future(),
		| CopyMode::Create => self
			.provider
			.copy_if_not_exists(&src, &dst)
			.right_future(),
	}
	.map_err(Error::from)
	.await
}

/// Streams object metadata beneath an optional logical prefix.
///
/// The configured provider prefix is applied to the backend query and removed
/// from each returned location. Backend failures remain stream items.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		provider = %self.name,
		?prefix,
	)
)]
pub fn list(&self, prefix: Option<&str>) -> impl Stream<Item = Result<ObjectMeta>> + Send {
	let abs_prefix = prefix
		.map(Path::from)
		.map(|p| self.prepend_base_path(p))
		.or_else(|| self.base_path.clone());

	self.provider
		.list(abs_prefix.as_ref())
		.map_err(Error::from)
		.map_ok(|meta| ObjectMeta {
			location: self.strip_base_path(meta.location),
			..meta
		})
}

/// Returns metadata for one object.
///
/// The provider prefix is applied before the backend request. Missing objects
/// and backend failures are propagated.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "debug"),
	skip_all,
	fields(
		provider = %self.name,
		?path,
	)
)]
pub async fn head(&self, path: &str) -> Result<ObjectMeta> {
	self.provider
		.head(&self.to_abs_path(path)?)
		.map_err(Error::from)
		.await
}

/// Probes whether this provider can service a listing request.
///
/// The probe consumes at most the first result, so an empty store succeeds.
/// Any path or backend error is logged and returned.
#[implement(Provider)]
#[tracing::instrument(
	level = "debug",
	err(level = "error"),
	skip_all,
	fields(
		provider = %self.name,
	)
)]
pub async fn ping(&self) -> Result {
	self.list(None)
		.try_next()
		.inspect_err(|e| {
			error!(chain = %error_chain(e), "Failed to connect to storage provider");
		})
		.boxed()
		.await
		.map(|_| ())
}

#[implement(Provider)]
fn to_abs_path(&self, location: &str) -> Result<Path> {
	let location = Path::parse(location)
		.map_err(|e| err!("Failed to parse location into canonical PathPart: {e}"))?;

	let path = self.prepend_base_path(location);

	trace!(
		provider = ?self.name,
		base_path = ?self.base_path,
		?path,
		"Computed absolute path for object on provider.",
	);

	Ok(path)
}

#[implement(Provider)]
fn prepend_base_path(&self, location: Path) -> Path {
	match self.base_path.as_ref() {
		| Some(base_path) if !location.prefix_matches(base_path) => base_path
			.parts()
			.chain(location.parts())
			.collect(),

		| _ => location,
	}
}

#[implement(Provider)]
fn strip_base_path(&self, location: Path) -> Path {
	self.base_path
		.as_ref()
		.and_then(|base_path| location.prefix_match(base_path))
		.map(Iterator::collect)
		.unwrap_or(location)
}

#[implement(Provider)]
fn multipart_threshold(&self) -> usize {
	extract_variant!(&self.config, StorageProvider::s3)
		.map(|config| config.multipart_threshold.as_u64())
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(usize::MAX)
}

#[implement(Provider)]
fn multipart_part_size(&self) -> usize {
	extract_variant!(&self.config, StorageProvider::s3)
		.map(|config| config.multipart_part_size.as_u64())
		.map(TryInto::try_into)
		.flat_ok()
		.unwrap_or(usize::MAX)
}

/// Splits a payload into nonempty parts no larger than `part_size`.
///
/// The iterator owns the payload buffer and advances it without copying the
/// bytes in each yielded part.
fn chunked(payload: PutPayload, part_size: usize) -> impl Iterator<Item = PutPayload> {
	let mut buf: Bytes = payload.into();
	from_fn(move || {
		buf.is_empty()
			.is_false()
			.then(|| buf.split_to(part_size.min(buf.len())).into())
	})
}

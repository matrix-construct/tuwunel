pub mod local;
pub mod s3;

use std::{iter::once, ops::Range, sync::Arc};

use bytes::Bytes;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use object_store::{
	Attributes, CopyMode, DynObjectStore, GetResult, MultipartUpload, ObjectMeta, ObjectStore,
	ObjectStoreExt, PutPayload, PutResult, path::Path,
};
use tuwunel_core::{
	Error, Result,
	config::StorageProvider,
	debug,
	derivative::Derivative,
	err, error, extract_variant, implement, info, trace,
	utils::{
		result::FlatOk,
		stream::{IterStream, TryReadyExt},
	},
};

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Provider {
	pub name: String,

	pub config: StorageProvider,

	pub(crate) provider: Box<DynObjectStore>,

	pub(crate) base_path: Option<Path>,

	startup_check: bool,

	#[expect(unused)]
	#[derivative(Debug = "ignore")]
	services: Arc<crate::services::OnceServices>,
}

pub type FetchItem = (Bytes, (Range<u64>, u64));
pub type FetchMetaItem = (Bytes, Arc<(Range<u64>, ObjectMeta, Attributes)>);

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

/// Put object into store from streaming input.
///
/// Recommended to know the total size of the object. If size is `None`,
/// multi-part upload may be selected even for small uploads below the
/// configured threshold.
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

/// Put object into the store from contiguous input.
///
/// The size of input will be determined and multipart upload will be chosen as
/// necessary internally.
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

	debug!(
		len = ?payload.content_length(),
		threshold = ?self.multipart_threshold(),
		"Selecting multi-part upload..."
	);

	self.put_multi(path, once(payload).try_stream())
		.await
}

/// Put object into the store from streaming input using multipart upload.
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
		.inspect_err(|e| error!(?path, "Failed to store object: {e:?}"))
		.await
	{
		| Ok(()) =>
			handle
				.complete()
				.map_err(Error::from)
				.inspect_err(|e| {
					error!(?path, "Failed to store object during completion: {e:?}");
				})
				.await,

		| Err(e) =>
			handle
				.abort()
				.map_ok(|()| Err(e))
				.map_err(Error::from)
				.inspect_err(|e| {
					error!(?path, "Additional errors during error handling: {e:?}");
				})
				.await?,
	}
}

/// Put object into the store from contiguous input non-multipart upload.
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
		.inspect_err(|e| error!("Failed to connect to storage provider: {e:?}"))
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
		.unwrap_or_default()
}

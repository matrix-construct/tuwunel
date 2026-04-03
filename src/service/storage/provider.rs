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
	err, error, implement, info, trace,
	utils::stream::{IterStream, TryReadyExt},
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
pub async fn store<S, T>(&self, path: &str, input: S) -> Result<PutResult>
where
	S: Stream<Item = Result<T>> + Send,
	PutPayload: From<T>,
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
pub async fn put<T>(&self, path: &str, input: T) -> Result<PutResult>
where
	PutPayload: From<T>,
{
	let path = self.to_abs_path(path)?;

	self.provider
		.put(&path, input.into())
		.map_err(Error::from)
		.await
}

#[implement(Provider)]
#[tracing::instrument(level = "debug", skip_all)]
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
#[tracing::instrument(level = "debug", skip_all)]
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
#[tracing::instrument(level = "debug", err(level = "debug"), skip_all)]
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
	self.provider
		.list(prefix.map(Into::into).as_ref())
		.map_err(Error::from)
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
	err(level = "debug"),
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
	let path_root = Path::ROOT;

	let base_path = self.base_path.as_ref().unwrap_or(&path_root);

	let location = Path::parse(location)
		.map_err(|e| err!("Failed to parse location into canonical PathPart: {e}"))?;

	let remaining = location.prefix_match(base_path);

	let path = base_path
		.into_iter()
		.chain(remaining.into_iter().flatten())
		.collect();

	trace!(
		provider = ?self.name,
		?base_path,
		?location,
		?path,
		"Computed absolute path for object on provider.",
	);

	Ok(path)
}

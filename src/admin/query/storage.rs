use std::collections::HashSet;

use futures::{FutureExt, StreamExt, TryStreamExt, future::try_join};
use tuwunel_core::{
	Result,
	utils::{
		result::LogErr,
		stream::{IterStream, TryBroadbandExt},
		string::SplitInfallible,
	},
};
use tuwunel_service::storage::CopyMode;

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch(handler_prefix = "query_storage")]
#[derive(Debug, clap::Subcommand)]
pub(crate) enum StorageCommand {
	/// List provider configurations.
	Configs,

	/// List provider instances.
	Providers,

	Debug {
		/// Use configured provider by name.
		provider: String,
	},

	/// Show metadata for an object.
	Show {
		/// Use configured provider by name.
		#[arg(short, long)]
		provider: Option<String>,

		/// Path to the object.
		src: String,
	},

	/// List metadata for all objects.
	List {
		/// Use configured provider by name.
		#[arg(short, long)]
		provider: Option<String>,

		/// Optionally filter by matching prefix.
		prefix: Option<String>,
	},

	/// List objects duplicated between two providers
	Duplicates {
		/// The first provider name.
		src: String,

		/// The second provider name.
		dst: String,
	},

	/// List objects duplicated between two providers
	Differences {
		/// The first provider name.
		src: String,

		/// The second provider name.
		dst: String,
	},

	/// Copy an object from a source to a destination.
	Copy {
		/// Use configured provider by name.
		#[arg(short, long)]
		provider: Option<String>,

		/// Overwrite existing destination.
		#[arg(short, long)]
		force: bool,

		/// Path to the source.
		src: String,

		/// Path to the destination.
		dst: String,
	},

	/// Move an object from a source to a destination.
	Move {
		/// Use configured provider by name.
		#[arg(short, long)]
		provider: Option<String>,

		/// Overwrite existing destination.
		#[arg(short, long)]
		force: bool,

		/// Path to the source.
		src: String,

		/// Path to the destination.
		dst: String,
	},

	/// Delete an object at the specified location.
	Delete {
		/// Use configured provider by name.
		#[arg(short, long)]
		provider: Option<String>,

		/// Path to the location to delete. Multiple arguments allowed.
		src: Vec<String>,

		/// Report successful results in addition to failures.
		#[arg(short, long)]
		verbose: bool,
	},

	/// Transfer objects from a source provider which do not exist on a
	/// destination provider.
	Sync {
		/// Use source configured provider by name.
		src: String,

		/// Use destination configured provider by name.
		dst: String,
	},
}

#[admin_command]
async fn query_storage_configs(&self) -> Result {
	self.services
		.storage
		.configs(None)
		.try_stream()
		.try_for_each(|(id, conf)| writeln!(&self, "\n`{id:?}`\n{conf:#?}"))
		.await
}

#[admin_command]
async fn query_storage_providers(&self) -> Result {
	self.services
		.storage
		.providers()
		.try_stream()
		.try_for_each(|conf| writeln!(&self, "\n`{:?}`\n{conf:#?}", conf.name))
		.await
}

#[admin_command]
async fn query_storage_debug(&self, provider: String) -> Result {
	let provider = self.services.storage.provider(&provider)?;

	self.write_string(format!("{provider:#?}\n"))
		.await
}

#[admin_command]
async fn query_storage_show(&self, provider: Option<String>, src: String) -> Result {
	let (prefix, src) = src.as_str().split_once_infallible("//");
	let id = provider.as_deref().unwrap_or(prefix);

	let provider = self.services.storage.provider(id)?;
	let meta = provider.head(src).await?;

	self.write_string(format!("{meta:#?}\n")).await
}

#[admin_command]
async fn query_storage_list(&self, provider: Option<String>, prefix: Option<String>) -> Result {
	let id = provider.as_deref().unwrap_or_default();
	let provider = self.services.storage.provider(id)?;

	provider
		.list(prefix.as_deref())
		.try_for_each(|meta| writeln!(&self, "{meta:?}"))
		.boxed()
		.await
}

#[admin_command]
async fn query_storage_duplicates(&self, provider_a: String, provider_b: String) -> Result {
	let a = self
		.services
		.storage
		.provider(&provider_a)?
		.list(None)
		.map_ok(|meta| meta.location)
		.try_collect::<HashSet<_>>();

	let b = self
		.services
		.storage
		.provider(&provider_b)?
		.list(None)
		.map_ok(|meta| meta.location)
		.try_collect::<HashSet<_>>();

	let (a, b) = try_join(a, b).await?;
	a.intersection(&b)
		.try_stream()
		.try_for_each(|item| writeln!(&self, "{item}"))
		.await
}

#[admin_command]
async fn query_storage_differences(&self, provider_a: String, provider_b: String) -> Result {
	let a = self
		.services
		.storage
		.provider(&provider_a)?
		.list(None)
		.map_ok(|meta| meta.location)
		.try_collect::<HashSet<_>>();

	let b = self
		.services
		.storage
		.provider(&provider_b)?
		.list(None)
		.map_ok(|meta| meta.location)
		.try_collect::<HashSet<_>>();

	let (a, b) = try_join(a, b).await?;
	a.difference(&b)
		.try_stream()
		.try_for_each(|item| writeln!(&self, "{item}"))
		.await
}

#[admin_command]
async fn query_storage_copy(
	&self,
	provider: Option<String>,
	force: bool,
	src: String,
	dst: String,
) -> Result {
	let id = provider.as_deref().unwrap_or_default();
	let provider = self.services.storage.provider(id)?;
	let overwrite = force
		.then_some(CopyMode::Overwrite)
		.unwrap_or(CopyMode::Create);

	let result = provider.copy(&src, &dst, overwrite).await;

	self.write_string(format!("{result:#?}\n")).await
}

#[admin_command]
async fn query_storage_move(
	&self,
	provider: Option<String>,
	force: bool,
	src: String,
	dst: String,
) -> Result {
	let id = provider.as_deref().unwrap_or_default();
	let provider = self.services.storage.provider(id)?;
	let overwrite = force
		.then_some(CopyMode::Overwrite)
		.unwrap_or(CopyMode::Create);

	let result = provider.rename(&src, &dst, overwrite).await;

	self.write_string(format!("{result:#?}\n")).await
}

#[admin_command]
async fn query_storage_delete(
	&self,
	provider: Option<String>,
	src: Vec<String>,
	verbose: bool,
) -> Result {
	let id = provider.as_deref().unwrap_or_default();
	let provider = self.services.storage.provider(id)?;

	provider
		.delete(src.into_iter().stream())
		.for_each(async |result| {
			match result {
				| Ok(_) if !verbose => Ok(()),
				| Ok(path) =>
					self.write_string(format!("deleted {path}\n"))
						.await,
				| Err(e) =>
					self.write_string(format!("failed: {e:?}\n"))
						.await,
			}
			.log_err()
			.ok();
		})
		.map(Ok)
		.await
}

#[admin_command]
async fn query_storage_sync(&self, src: String, dst: String) -> Result {
	let src_p = self.services.storage.provider(&src)?;

	let dst_p = self.services.storage.provider(&dst)?;

	let src = src_p
		.list(None)
		.map_ok(|meta| meta.location)
		.try_collect::<HashSet<_>>();

	let dst = dst_p
		.list(None)
		.map_ok(|meta| meta.location)
		.try_collect::<HashSet<_>>();

	let (src, dst) = try_join(src, dst).await?;

	src.difference(&dst)
		.try_stream()
		.broadn_and_then(2, async |item| {
			let data = src_p.get(item.as_ref()).await?;
			let put = dst_p.put(item.as_ref(), data).await?;

			Ok((item, put))
		})
		.try_for_each(|(item, put)| {
			writeln!(&self, "Moved {item} from {src:?} to {dst:?}; {put:?}")
		})
		.await
}

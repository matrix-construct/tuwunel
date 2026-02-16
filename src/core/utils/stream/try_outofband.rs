//! Out-of-band parallel stream combinator extensions to futures::Stream

use futures::{TryFutureExt, stream::TryStream};
use tokio::{runtime, task::JoinError};

use super::TryBroadbandExt;
use crate::{Error, Result, utils::sys::available_parallelism};

/// Parallelism extensions to augment futures::StreamExt. These combinators are
/// for computation-oriented workloads, unlike -band combinators for I/O
/// workloads; these default to the available compute parallelism for the
/// system.
///
/// Threads are currently drawn from the tokio-spawn pool. Each stream item
/// executes fully independently and immediately. Results are unordered.
pub trait TryOutOfBandExt<T, E>
where
	Self: TryStream<Ok = T, Error = E, Item = Result<T, E>> + Send + Sized,
	E: From<JoinError> + From<Error> + Send + 'static,
	T: Send + 'static,
{
	fn out_of_bandn_and_then<U, F, N, H>(
		self,
		h: H,
		n: N,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		N: Into<Option<usize>>,
		H: Into<Option<runtime::Handle>>,
		F: Fn(Self::Ok) -> Result<U, E> + Clone + Send + 'static,
		U: Send + 'static;

	fn out_of_band_and_then<U, F, H>(
		self,
		h: H,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		H: Into<Option<runtime::Handle>>,
		F: Fn(Self::Ok) -> Result<U, E> + Clone + Send + 'static,
		U: Send + 'static,
	{
		self.out_of_bandn_and_then(h, None, f)
	}
}

impl<T, E, S> TryOutOfBandExt<T, E> for S
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + Send + Sized,
	E: From<JoinError> + From<Error> + Send + 'static,
	T: Send + 'static,
{
	fn out_of_bandn_and_then<U, F, N, H>(
		self,
		h: H,
		n: N,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		N: Into<Option<usize>>,
		H: Into<Option<runtime::Handle>>,
		F: Fn(Self::Ok) -> Result<U, E> + Clone + Send + 'static,
		U: Send + 'static,
	{
		let n = n.into().unwrap_or_else(available_parallelism);
		let h = h.into().unwrap_or_else(runtime::Handle::current);
		self.broadn_and_then(n, move |val| {
			let (h, f) = (h.clone(), f.clone());
			async move {
				h.spawn_blocking(move || f(val))
					.map_err(E::from)
					.await?
			}
		})
	}
}

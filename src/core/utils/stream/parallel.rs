//! Parallel stream combinator extensions to futures::TryStream

use futures::{Stream, StreamExt};
use tokio::runtime;

use super::{BroadbandExt, IterStream, automatic_width};
use crate::{expected, utils::sys::available_parallelism};

/// Parallelism extensions to augment futures::StreamExt. These combinators
/// accelerate workloads which could benefit from parallel execution on multiple
/// cores, but are otherwise cooperatively asynchronous without blocking the
/// event-loop. For computation-oriented workloads, use the `TryOutOfBandExt`
/// interface instead. These combinators are for extremely large I/O-bound tasks
/// or for modestly compute-heavy tasks sufficiently interleaved with I/O.
///
/// Stream items are not always independent as they may be coalesced into
/// batches for each tokio-task which can then be executed in parallel at the
/// discretion of the executor's work-stealing. Stream items still execute
/// concurrently within each batch. This approach reduces overhead for small
/// workloads while progressively ramping up for larger ones.
pub trait ParallelExt<T>
where
	Self: Stream<Item = T> + Send + Sized,
	T: Send,
{
	/// Process stream items in parallel
	///
	/// Inputs:
	/// - handle: Optional handle to the runtime; None to derive automatically.
	/// - parallelism: Optional parallelism factor; None to use
	///   `available_parallelism()`.
	/// - concurrency: Optional concurrency factor; None to derive based on
	///   stream width.
	/// - batch_size: Optional batch size; None to derive based on stream
	///   amplification.
	/// - func: Function to process each item.
	fn paralleln_then<U, Fut, F, N, B, P, H>(
		self,
		handle: H,
		parallelism: P,
		batch_size: B,
		concurrency: N,
		func: F,
	) -> impl Stream<Item = U> + Send
	where
		H: Into<Option<runtime::Handle>>,
		P: Into<Option<usize>>,
		B: Into<Option<usize>>,
		N: Into<Option<usize>>,
		F: Fn(T) -> Fut + Clone + Send + Sync + 'static,
		Fut: Future<Output = U> + Send,
		U: Send + 'static,
		T: 'static;

	/// Process stream items in parallel
	///
	/// Inputs:
	/// - handle: Optional handle to the runtime; None to derive automatically.
	/// - parallelism: Optional parallelism factor; None to use
	///   `available_parallelism()`.
	/// - concurrency: Optional concurrency factor; None to derive based on
	///   stream width.
	/// - batch_size: Optional batch size; None to derive based on stream
	///   amplification.
	/// - func: Function to process each item.
	fn paralleln_for_each<Fut, F, N, B, P, H>(
		self,
		handle: H,
		parallelism: P,
		batch_size: B,
		concurrency: N,
		func: F,
	) -> impl Future<Output = ()> + Send
	where
		H: Into<Option<runtime::Handle>>,
		P: Into<Option<usize>>,
		B: Into<Option<usize>>,
		N: Into<Option<usize>>,
		F: FnMut(T) -> Fut + Clone + Send + Sync + 'static,
		Fut: Future<Output = ()> + Send,
		T: 'static;

	/// Process stream items in parallel
	///
	/// Inputs:
	/// - handle: Optional handle to the runtime; None to derive automatically.
	/// - func: Function to process each item.
	fn parallel_then<U, Fut, F, H>(self, handle: H, func: F) -> impl Stream<Item = U> + Send
	where
		H: Into<Option<runtime::Handle>>,
		F: Fn(T) -> Fut + Clone + Send + Sync + 'static,
		Fut: Future<Output = U> + Send,
		U: Send + 'static,
		T: 'static,
	{
		self.paralleln_then(handle, None, None, None, func)
	}

	/// Process stream items in parallel
	///
	/// Inputs:
	/// - handle: Optional handle to the runtime; None to derive automatically.
	/// - func: Function to process each item.
	fn parallel_for_each<Fut, F, H>(self, handle: H, func: F) -> impl Future<Output = ()> + Send
	where
		H: Into<Option<runtime::Handle>>,
		F: FnMut(T) -> Fut + Clone + Send + Sync + 'static,
		Fut: Future<Output = ()> + Send,
		T: 'static,
	{
		self.paralleln_for_each(handle, None, None, None, func)
	}
}

impl<T, S> ParallelExt<T> for S
where
	S: Stream<Item = T> + Send + Sized,
	T: Send,
{
	fn paralleln_for_each<Fut, F, N, B, P, H>(
		self,
		handle: H,
		parallelism: P,
		batch_size: B,
		concurrency: N,
		func: F,
	) -> impl Future<Output = ()> + Send
	where
		H: Into<Option<runtime::Handle>>,
		P: Into<Option<usize>>,
		B: Into<Option<usize>>,
		N: Into<Option<usize>>,
		F: FnMut(T) -> Fut + Clone + Send + Sync + 'static,
		Fut: Future<Output = ()> + Send,
		T: 'static,
	{
		let concurrency = concurrency.into().unwrap_or_else(automatic_width);

		let parallelism = parallelism
			.into()
			.unwrap_or_else(available_parallelism);

		let batch_size = batch_size
			.into()
			.unwrap_or_else(|| expected!(concurrency * 2));

		let handle = handle
			.into()
			.unwrap_or_else(runtime::Handle::current);

		self.ready_chunks(batch_size)
			.map(move |batch| (batch, handle.clone(), func.clone(), concurrency))
			.for_each_concurrent(parallelism, async |(batch, handle, func, concurrency)| {
				handle
					.spawn(async move {
						batch
							.into_iter()
							.stream()
							.for_each_concurrent(concurrency, func)
							.await;
					})
					.await
					.expect("Failed to join ParallelExt batch task");
			})
	}

	fn paralleln_then<U, Fut, F, N, B, P, H>(
		self,
		handle: H,
		parallelism: P,
		batch_size: B,
		concurrency: N,
		func: F,
	) -> impl Stream<Item = U> + Send
	where
		H: Into<Option<runtime::Handle>>,
		P: Into<Option<usize>>,
		B: Into<Option<usize>>,
		N: Into<Option<usize>>,
		F: Fn(T) -> Fut + Clone + Send + Sync + 'static,
		Fut: Future<Output = U> + Send,
		U: Send + 'static,
		T: 'static,
	{
		let concurrency = concurrency.into().unwrap_or_else(automatic_width);

		let parallelism = parallelism
			.into()
			.unwrap_or_else(available_parallelism);

		let batch_size = batch_size
			.into()
			.unwrap_or_else(|| expected!(concurrency * 2));

		let handle = handle
			.into()
			.unwrap_or_else(runtime::Handle::current);

		self.ready_chunks(batch_size)
			.map(move |batch| (batch, handle.clone(), func.clone(), concurrency))
			.broadn_then(parallelism, async |(batch, handle, func, concurrency)| {
				handle
					.spawn(async move {
						batch
							.into_iter()
							.stream()
							.broadn_then(concurrency, func)
							.collect::<Vec<_>>()
							.await
					})
					.await
					.expect("Failed to join ParallelExt batch task")
					.into_iter()
					.stream()
			})
			.flatten()
	}
}

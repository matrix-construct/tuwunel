//! Extended external extensions to futures::FutureExt

use std::marker::Unpin;

use futures::{
	future::{select_ok, try_join, try_join_all, try_select},
	Future, FutureExt,
};

pub trait BoolExt
where
	Self: Future<Output = bool> + Send,
{
	fn and<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized;

	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send + Unpin,
		Self: Sized + Unpin;
}

pub async fn and<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send,
{
	type Result = crate::Result<(), ()>;

	let args = args.map(|a| a.map(|a| a.then_some(()).ok_or(Result::Err(()))));

	try_join_all(args).map(|result| result.is_ok())
}

pub async fn or<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send + Unpin,
{
	type Result = crate::Result<(), ()>;

	let args = args.map(|a| a.map(|a| a.then_some(()).ok_or(Result::Err(()))));

	select_ok(args).map(|result| result.is_ok())
}

impl<Fut> BoolExt for Fut
where
	Fut: Future<Output = bool> + Send,
{
	#[inline]
	fn and<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized,
	{
		type Result = crate::Result<(), ()>;

		let a = self.map(|a| a.then_some(()).ok_or(Result::Err(())));

		let b = b.map(|b| b.then_some(()).ok_or(Result::Err(())));

		try_join(a, b).map(|result| result.is_ok())
	}

	#[inline]
	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send + Unpin,
		Self: Sized + Unpin,
	{
		type Result = crate::Result<(), ()>;

		let a = self.map(|a| a.then_some(()).ok_or(Result::Err(())));

		let b = b.map(|b| b.then_some(()).ok_or(Result::Err(())));

		try_select(a, b).map(|result| result.is_ok())
	}
}

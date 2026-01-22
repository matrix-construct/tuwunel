//! Extended external extensions to futures::FutureExt
#![expect(clippy::many_single_char_names, clippy::impl_trait_in_params)]

use std::marker::Unpin;

use futures::{
	Future, FutureExt,
	future::{
		Either::{Left, Right},
		select_ok, try_join, try_join_all, try_join3, try_join4,
	},
};

use crate::utils::BoolExt as _;

pub trait BoolExt
where
	Self: Future<Output = bool> + Send,
{
	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send + Unpin,
		Self: Sized + Unpin;

	fn and<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized;

	fn and2<B, C>(self, b: B, c: C) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		Self: Sized;

	fn and3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized;
}

impl<Fut> BoolExt for Fut
where
	Fut: Future<Output = bool> + Send,
{
	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send + Unpin,
		Self: Sized + Unpin,
	{
		select_ok([Left(self.map(test)), Right(b.map(test))]).map(|res| res.is_ok())
	}

	fn and<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join(self.map(test), b.map(test)).map(|res| res.is_ok())
	}

	fn and2<B, C>(self, b: B, c: C) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join3(self.map(test), b.map(test), c.map(test)).map(|res| res.is_ok())
	}

	fn and3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join4(self.map(test), b.map(test), c.map(test), d.map(test)).map(|res| res.is_ok())
	}
}

pub fn and<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send,
{
	let args = args.map(|a| a.map(test));

	try_join_all(args).map(|res| res.is_ok())
}

pub fn or<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send + Unpin,
{
	let args = args.map(|a| a.map(test));

	select_ok(args).map(|res| res.is_ok())
}

pub fn and4(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and3(b, c, d)
}

pub fn and5(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
	e: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and2(b, c).and2(d, e)
}

pub fn and6(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
	e: impl Future<Output = bool> + Send,
	f: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and3(b, c, d).and2(e, f)
}

pub fn and7(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
	e: impl Future<Output = bool> + Send,
	f: impl Future<Output = bool> + Send,
	g: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and3(b, c, d).and3(e, f, g)
}

fn test(test: bool) -> crate::Result<(), ()> { test.ok_or(()) }

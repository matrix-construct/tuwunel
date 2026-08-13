mod items;
mod items_rev;
mod keys;
mod keys_rev;

use std::{mem::replace, sync::Arc};

use rocksdb::{DBRawIteratorWithThreadMode, ReadOptions};
use tuwunel_core::Result;

pub(crate) use self::{items::Items, items_rev::ItemsRev, keys::Keys, keys_rev::KeysRev};
use crate::{
	Map, Slice,
	engine::Db,
	keyval::{Key, KeyVal, Val},
	util::{is_incomplete, map_err},
};

/// Owns a RocksDB raw iterator and its initial positioning state.
///
/// The iterator borrows the map's engine for `'a`. The flags distinguish the
/// first poll from later cursor movement and record whether an explicit seek
/// position has already been chosen.
pub(crate) struct State<'a> {
	inner: Inner<'a>,
	seek: bool,
	init: bool,
}

/// Defines the polling operations shared by database cursor streams.
///
/// Implementations position a [`State`], fetch one borrowed item, and surface
/// RocksDB cursor errors. Borrowed items remain valid only until the next
/// cursor movement and must be owned before they are retained.
pub(crate) trait Cursor<'a, T>: Send {
	fn state(&self) -> &State<'a>;

	fn state_mut(&mut self) -> &mut State<'a>;

	fn count(&self) -> (usize, Option<usize>);

	fn fetch(&self) -> Option<T>;

	fn seek(&mut self);

	#[inline]
	fn get(&self) -> Option<Result<T>> {
		self.fetch()
			.map(Ok)
			.or_else(|| self.state().status().map(map_err).map(Err))
	}

	#[inline]
	fn seek_and_get(&mut self) -> Option<Result<T>> {
		self.seek();
		self.get()
	}
}

type Inner<'a> = DBRawIteratorWithThreadMode<'a, Db>;
type From<'a> = Option<Key<'a>>;

impl<'a> State<'a> {
	#[inline]
	pub(super) fn new(map: &'a Arc<Map>, opts: ReadOptions) -> Self {
		Self {
			init: true,
			seek: false,
			inner: map
				.engine()
				.db
				.raw_iterator_cf_opt(&map.cf(), opts),
		}
	}

	#[inline]
	#[tracing::instrument(level = "trace", skip_all)]
	pub(super) fn init_fwd(mut self, from: From<'_>) -> Self {
		debug_assert!(self.init, "init must be set to make this call");
		debug_assert!(!self.seek, "seek must not be set to make this call");

		if let Some(key) = from {
			self.inner.seek(key);
		} else {
			self.inner.seek_to_first();
		}

		self.seek = true;
		self
	}

	#[inline]
	#[tracing::instrument(level = "trace", skip_all)]
	pub(super) fn init_rev(mut self, from: From<'_>) -> Self {
		debug_assert!(self.init, "init must be set to make this call");
		debug_assert!(!self.seek, "seek must not be set to make this call");

		if let Some(key) = from {
			self.inner.seek_for_prev(key);
		} else {
			self.inner.seek_to_last();
		}

		self.seek = true;
		self
	}

	#[inline]
	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	pub(super) fn seek_fwd(&mut self) {
		if !replace(&mut self.init, false) {
			self.inner.next();
		} else if !self.seek {
			self.inner.seek_to_first();
		}
	}

	#[inline]
	#[cfg_attr(unabridged, tracing::instrument(level = "trace", skip_all))]
	pub(super) fn seek_rev(&mut self) {
		if !replace(&mut self.init, false) {
			self.inner.prev();
		} else if !self.seek {
			self.inner.seek_to_last();
		}
	}

	#[inline]
	#[expect(clippy::unused_self)]
	#[tracing::instrument(level = "trace", skip_all, ret)]
	pub(super) fn count_fwd(&self) -> (usize, Option<usize>) { (0, None) }

	#[inline]
	#[expect(clippy::unused_self)]
	#[tracing::instrument(level = "trace", skip_all, ret)]
	pub(super) fn count_rev(&self) -> (usize, Option<usize>) { (0, None) }

	#[inline]
	fn fetch_key(&self) -> Option<Key<'_>> { self.inner.key() }

	#[inline]
	fn _fetch_val(&self) -> Option<Val<'_>> { self.inner.value() }

	#[inline]
	fn fetch(&self) -> Option<KeyVal<'_>> { self.inner.item() }

	pub(super) fn is_incomplete(&self) -> bool {
		matches!(self.status(), Some(e) if is_incomplete(&e))
	}

	#[inline]
	pub(super) fn status(&self) -> Option<rocksdb::Error> { self.inner.status().err() }

	#[inline]
	pub(super) fn valid(&self) -> bool { self.inner.valid() }
}

/// Extends both borrows in a cursor key-value pair to the stream lifetime.
///
/// The returned pair remains valid only until the next cursor movement. This
/// helper delegates each component to [`slice_longevity`] and carries the same
/// lifetime contract.
fn keyval_longevity<'a, 'b: 'a>(item: KeyVal<'a>) -> KeyVal<'b> {
	(slice_longevity::<'a, 'b>(item.0), slice_longevity::<'a, 'b>(item.1))
}

/// Extends a RocksDB cursor slice borrow to the stream lifetime.
///
/// The extension bridges `Stream`'s fixed item type and does not extend the
/// underlying storage validity. The returned reference becomes invalid when the
/// cursor next moves, so callers must not retain it across a poll.
fn slice_longevity<'a, 'b: 'a>(item: &'a Slice) -> &'b Slice {
	// SAFETY: The lifetime of the data returned by the rocksdb cursor is only valid
	// between each movement of the cursor. It is hereby unsafely extended to match
	// the lifetime of the cursor itself. This is due to the limitation of the
	// Stream trait where the Item is incapable of conveying a lifetime; this is due
	// to GAT's being unstable during its development. This unsafety can be removed
	// as soon as this limitation is addressed by an upcoming version.
	//
	// We have done our best to mitigate the implications of this in conjunction
	// with the deserialization API such that borrows being held across movements of
	// the cursor do not happen accidentally. The compiler will still error when
	// values herein produced try to leave a closure passed to a StreamExt API. But
	// escapes can happen if you explicitly and intentionally attempt it, and there
	// will be no compiler error or warning. This is primarily the case with
	// calling collect() without a preceding map(ToOwned::to_owned). A collection
	// of references here is illegal, but this will not be enforced by the compiler.
	unsafe { std::mem::transmute(item) }
}

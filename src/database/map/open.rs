use std::sync::Arc;

use rocksdb::ColumnFamily;

use crate::Engine;

/// Acquires a stable column-family handle for a map.
///
/// The returned handle erases the engine-borrow lifetime carried by RocksDB.
/// Its caller must retain the engine until after the handle is dropped, which
/// `Map` guarantees by field ownership.
///
/// # Panics
///
/// Panics if the column family was not described before the engine opened.
pub(super) fn open(engine: &Arc<Engine>, name: &str) -> Arc<ColumnFamily> {
	let bounded_arc = engine.cf(name);
	let bounded_ptr = Arc::into_raw(bounded_arc);
	let cf_ptr = bounded_ptr.cast::<ColumnFamily>();

	// SAFETY: Column family handles out of RocksDB are basic pointers and can
	// be invalidated: 1. when the database closes. 2. when the column is dropped or
	// closed. rust_rocksdb wraps this for us by storing handles in their own
	// `RwLock<BTreeMap>` map and returning an Arc<BoundColumnFamily<'_>>` to
	// provide expected safety. Similarly in "single-threaded mode" we would
	// receive `&'_ ColumnFamily`.
	//
	// PROBLEM: We need to hold these handles in a field, otherwise we have to take
	// a lock and get them by name from this map for every query, which is what
	// conduit was doing, but we're not going to make a query for every query so we
	// need to be holding it right. The lifetime parameter on these references makes
	// that complicated. If this can be done without polluting the userspace
	// with lifetimes on every instance of `Map` then this `unsafe` might not be
	// necessary.
	//
	// SOLUTION: After investigating the underlying types it appears valid to
	// Arc-swap `BoundColumnFamily<'_>` for `ColumnFamily`. They have the
	// same inner data, the same Drop behavior, Deref, etc. We're just losing the
	// lifetime parameter. We should not hold this handle, even in its Arc, after
	// closing the database (dropping `Engine`). Since `Arc<Engine>` is a sibling
	// member along with this handle in `Map`, that is prevented.
	unsafe { Arc::from_raw(cf_ptr) }
}

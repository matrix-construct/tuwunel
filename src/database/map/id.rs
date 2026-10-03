use crate::{engine::descriptor::Descriptor, maps::MAPS};

/// Identifies a column by its position in the database catalog.
///
/// Construct handles with [`map!`](crate::map!) to check names at compile time.
/// A handle identifies the same catalog position in every database instance.
#[derive(Clone, Copy, Debug)]
pub struct MapId(pub(crate) usize);

/// Resolves a column name into a compile-time catalog handle.
///
/// Unknown names fail constant evaluation. Indexing a database still panics if
/// the catalog entry was dropped or its column family is unavailable.
#[macro_export]
macro_rules! map {
	($name:expr) => {
		const { $crate::MapId::named($name) }
	};
}

impl MapId {
	/// Finds a column's position in the database catalog.
	///
	/// Names are compared byte for byte. An unknown name panics with that name,
	/// which becomes a build error when evaluated in a constant context.
	#[must_use]
	pub const fn named(name: &str) -> Self {
		let mut index = 0; // Const iteration requires a cursor.

		while index < MAPS.len() {
			if MAPS[index].name.len() == name.len() && matches(&MAPS[index], name.as_bytes()) {
				return Self(index);
			}

			index = index.saturating_add(1);
		}

		panic!("{}", name);
	}
}

const fn matches(desc: &Descriptor, name: &[u8]) -> bool {
	let mut index = 0; // Const iteration requires a cursor.

	while index < desc.name.len() {
		if desc.name.as_bytes()[index] != name[index] {
			return false;
		}

		index = index.saturating_add(1);
	}

	true
}

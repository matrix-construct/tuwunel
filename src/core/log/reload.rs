use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use tracing_subscriber::{EnvFilter, reload};

use crate::{Result, error};

/// Type-erased interface to a tracing subscriber reload handle.
///
/// The subscriber type in `reload::Handle<L, S>` depends on preceding layers
/// and can include unnameable `impl Trait` types. This interface hides `S` so
/// handles can be stored as trait objects.
pub trait ReloadHandle<L> {
	/// Clones the filter currently installed through this handle.
	///
	/// A missing value indicates that the subscriber or reload layer is no
	/// longer available. The type-erased interface preserves the concrete
	/// layer value.
	fn current(&self) -> Option<L>;

	/// Replaces the layer value controlled by this handle.
	///
	/// Reloading affects future subscriber decisions without reconstructing the
	/// subscriber stack. The underlying reload layer reports unavailable state.
	fn reload(&self, new_value: L) -> Result<(), reload::Error>;
}

impl<L: Clone, S> ReloadHandle<L> for reload::Handle<L, S> {
	fn current(&self) -> Option<L> { Self::clone_current(self) }

	fn reload(&self, new_value: L) -> Result<(), reload::Error> { Self::reload(self, new_value) }
}

/// Named collection of type-erased log-filter reload handles.
///
/// Clones share the same synchronized handle map. Names let administrative and
/// scoped operations target individual subscriber layers.
#[derive(Clone)]
pub struct LogLevelReloadHandles {
	handles: Arc<Mutex<HandleMap>>,
}

type HandleMap = HashMap<String, Handle>;
type Handle = Box<dyn ReloadHandle<EnvFilter> + Send + Sync>;

impl LogLevelReloadHandles {
	/// Registers or replaces a reload handle under a name.
	///
	/// Later calls to `reload` and `current` address the handle by this name.
	/// The handle remains owned by the shared collection.
	///
	/// # Panics
	///
	/// Panics if the shared handle map mutex is poisoned.
	pub fn add(&self, name: &str, handle: Handle) {
		self.handles
			.lock()
			.expect("locked")
			.insert(name.into(), handle);
	}

	/// Applies a log filter to the selected named handles.
	///
	/// Only handles whose names occur in the supplied slice are changed; `None`
	/// selects no handles. Individual reload failures are logged and do not
	/// stop other handles.
	///
	/// # Panics
	///
	/// Panics if the shared handle map mutex is poisoned.
	pub fn reload(&self, new_value: &EnvFilter, names: Option<&[&str]>) -> Result {
		self.handles
			.lock()
			.expect("locked")
			.iter()
			.filter(|(name, _)| names.is_some_and(|names| names.contains(&name.as_str())))
			.for_each(|(_, handle)| {
				_ = handle
					.reload(new_value.clone())
					.or_else(error::else_log);
			});

		Ok(())
	}

	/// Returns the current filter for a named handle.
	///
	/// Missing names and unavailable reload layers both produce `None`. The
	/// returned filter is cloned from the layer.
	///
	/// # Panics
	///
	/// Panics if the shared handle map mutex is poisoned.
	#[must_use]
	pub fn current(&self, name: &str) -> Option<EnvFilter> {
		self.handles
			.lock()
			.expect("locked")
			.get(name)
			.map(|handle| handle.current())?
	}
}

impl Default for LogLevelReloadHandles {
	fn default() -> Self {
		Self {
			handles: Arc::new(HandleMap::new().into()),
		}
	}
}

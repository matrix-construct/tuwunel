use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};

use tracing_subscriber::{EnvFilter, reload};

use crate::{Result, error};

/// We need to store a reload::Handle value, but can't name it's type explicitly
/// because the S type parameter depends on the subscriber's previous layers. In
/// our case, this includes unnameable 'impl Trait' types.
///
/// This is fixed[1] in the unreleased tracing-subscriber from the master
/// branch, which removes the S parameter. Unfortunately can't use it without
/// pulling in a version of tracing that's incompatible with the rest of our
/// deps.
///
/// To work around this, we define an trait without the S paramter that forwards
/// to the reload::Handle::reload method, and then store the handle as a trait
/// object.
///
/// [1]: <https://github.com/tokio-rs/tracing/pull/1035/commits/8a87ea52425098d3ef8f56d92358c2f6c144a28f>
pub trait ReloadHandle<L> {
	fn current(&self) -> Option<L>;

	fn reload(&self, new_value: L) -> Result<(), reload::Error>;
}

impl<L: Clone, S> ReloadHandle<L> for reload::Handle<L, S> {
	fn current(&self) -> Option<L> {
		Self::clone_current(self)
	}

	fn reload(&self, new_value: L) -> Result<(), reload::Error> {
		Self::reload(self, new_value)
	}
}

#[derive(Clone)]
pub struct LogLevelReloadHandles {
	handles: Arc<Mutex<HandleMap>>,
}

type HandleMap = HashMap<String, Handle>;
type Handle = Box<dyn ReloadHandle<EnvFilter> + Send + Sync>;

impl LogLevelReloadHandles {
	pub fn add(&self, name: &str, handle: Handle) {
		self.handles
			.lock()
			.expect("locked")
			.insert(name.into(), handle);
	}

	pub fn reload(&self, new_value: &EnvFilter, names: Option<&[&str]>) -> Result<()> {
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

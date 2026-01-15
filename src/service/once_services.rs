use std::{
	ops::Deref,
	sync::{Arc, OnceLock},
};

use crate::Services;

#[derive(Default)]
pub(crate) struct OnceServices {
	lock: OnceLock<Arc<Services>>,
}

impl OnceServices {
	pub(super) fn set(&self, services: Arc<Services>) -> Arc<Services> {
		self.lock.get_or_init(move || services).clone()
	}

	#[inline]
	pub(crate) fn get(&self) -> &Arc<Services> {
		self.lock
			.get()
			.expect("services must be initialized")
	}
}

impl Deref for OnceServices {
	type Target = Arc<Services>;

	#[inline]
	fn deref(&self) -> &Self::Target { self.get() }
}

#[cfg(not(tuwunel_always_prove_sendness))]
// SAFETY: Services has a lot of circularity inherited from Conduit's original
// design. This stresses the trait solver which twists itself into a knot
// proving Sendness. This issue was a lot worse in conduwuit where we used an
// instance of `Dep` for each Service rather than a single instance of
// `OnceServices` like now. The problem still exists though greatly reduced, and
// the same solution now has greater impact because OnceServices is the single
// unified focal-point for the entire Services call-web.
//
// The prior incarnation required this unsafety or it would blow through the
// recursion_limit; that no longer happens. Nevertheless compile times are
// still substantially reduced by asserting Sendness here. Prove sendness
// by simply commenting this out or using `--cfg tuwunel_always_prove_sendness`,
// it will just take longer.
unsafe impl Send for OnceServices {}

#[cfg(not(tuwunel_always_prove_syncness))]
// SAFETY: Similar to Send as explained above, we further reduce compile-times
// by manually asserting Syncness of this type. The only threading contention
// concerns for this would be on startup but this server has a very well defined
// initialization sequence. After that this structure is purely read-only shared
// without concern.
//
// Proof can be verified by using `--cfg tuwunel_always_prove_syncness` at the
// cost of additional build time.
unsafe impl Sync for OnceServices {}

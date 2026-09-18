//! Internal failure reporting for federation fetches.
//!
//! Failures remain cloneable for broadcast to coalesced callers and retain the
//! servers actually contacted for operator-facing diagnostics.

use std::fmt;

use ruma::OwnedServerName;
use tuwunel_core::{err, smallvec::SmallVec};

/// Servers contacted before a fetch gave up.
///
/// Inline storage is sized to the common candidate-pool budget.
pub(super) type Attempted = SmallVec<[OwnedServerName; 3]>;

/// Describes why a coalesced fetch produced no response.
///
/// The cloneable value is broadcast to every subscriber and converted to
/// [`tuwunel_core::Error`] at the public boundary.
#[derive(Clone, Debug)]
pub(super) enum Failure {
	/// The permitted attempts or rounds ended without a valid response.
	NotFound {
		/// Servers actually contacted before the fetch stopped.
		attempted: Attempted,
	},

	/// No candidate servers were available to try.
	NoCandidates,

	/// Caller interest vanished or worker communication closed before a response arrived.
	Cancelled,
}

impl fmt::Display for Failure {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::NoCandidates => write!(f, "no candidate servers available"),
			| Self::Cancelled => write!(f, "fetch cancelled"),
			| Self::NotFound { attempted } => {
				write!(f, "event not found on any of {} servers", attempted.len())
			},
		}
	}
}

impl From<Failure> for tuwunel_core::Error {
	fn from(failure: Failure) -> Self { err!(Request(NotFound("{failure}"))) }
}

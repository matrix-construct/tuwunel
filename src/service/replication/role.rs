//! Which role this process is in right now: the primary, or a replica of one.
//!
//! The rule lives here, once. Everything that must behave differently on a
//! replica — the replication status endpoint, the outbound sender — asks
//! [`super::Service::role`] rather than reading the config itself, so a
//! runtime `promote()` or `demote()` is seen the same way everywhere.

/// The role of this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
	/// Takes client traffic and writes; does the outbound sending.
	Primary,
	/// Tails a primary's WAL. Its database is the primary's; it must not act
	/// on that data (send transactions, push, ...) on its own.
	Replica,
}

impl Role {
	/// The one rule.
	///
	/// - `configured_primary`: `rocksdb_primary_url` is set in the config, so
	///   the process started as a replica.
	/// - `demoted`: `demote()` has given this process a primary to follow at
	///   runtime (that URL is never cleared again).
	/// - `promoted`: `promote()` has made this process a primary. It wins over
	///   both of the above until a later `demote()` clears it.
	#[must_use]
	pub const fn derive(configured_primary: bool, demoted: bool, promoted: bool) -> Self {
		if promoted {
			Self::Primary
		} else if configured_primary || demoted {
			Self::Replica
		} else {
			Self::Primary
		}
	}

	#[inline]
	#[must_use]
	pub const fn is_replica(self) -> bool { matches!(self, Self::Replica) }

	/// The name `/_tuwunel/replication/status` reports.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			| Self::Primary => "primary",
			| Self::Replica => "secondary",
		}
	}
}

#[cfg(test)]
mod tests {
	use super::Role;

	#[test]
	fn standalone_primary() {
		assert_eq!(Role::derive(false, false, false), Role::Primary);
	}

	#[test]
	fn configured_replica() {
		assert_eq!(Role::derive(true, false, false), Role::Replica);
	}

	#[test]
	fn promoted_replica_is_primary() {
		assert_eq!(Role::derive(true, false, true), Role::Primary);
	}

	#[test]
	fn demoted_standalone_primary_is_replica() {
		// Started without `rocksdb_primary_url`, demoted at runtime.
		assert_eq!(Role::derive(false, true, false), Role::Replica);
	}

	#[test]
	fn demoted_then_promoted_again_is_primary() {
		assert_eq!(Role::derive(false, true, true), Role::Primary);
		assert_eq!(Role::derive(true, true, true), Role::Primary);
	}

	#[test]
	fn promoted_then_demoted_is_replica() {
		// `demote()` clears `promoted` and sets the runtime URL.
		assert_eq!(Role::derive(true, true, false), Role::Replica);
	}

	#[test]
	fn status_names() {
		assert_eq!(Role::Primary.as_str(), "primary");
		assert_eq!(Role::Replica.as_str(), "secondary");
		assert!(Role::Replica.is_replica());
		assert!(!Role::Primary.is_replica());
	}
}

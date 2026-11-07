/// What a repair made of one finding.
///
/// A repair reads its row again before writing, so a finding can turn out stale by
/// the time it is acted on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Outcome {
	/// The row was rewritten.
	Repaired,

	/// The row was deleted.
	Removed,

	/// The row was left alone by design, such as one the check cannot read.
	Skipped,

	/// The row was no longer defective, or gone, when read again.
	Stale,
}

/// How many findings of a pass came to each outcome.
///
/// Each finding's repair returns its own outcome, so repairs can run concurrently
/// and the totals still add up.
#[derive(Default)]
pub(super) struct Tally {
	/// Findings whose row was rewritten.
	pub(super) repaired: usize,

	/// Findings whose row was deleted.
	pub(super) removed: usize,

	/// Findings left alone by design.
	pub(super) skipped: usize,

	/// Findings stale by the time they were acted on.
	pub(super) stale: usize,
}

impl Tally {
	pub(super) fn record(mut self, outcome: Outcome) -> Self {
		let count = match outcome {
			| Outcome::Repaired => &mut self.repaired,
			| Outcome::Removed => &mut self.removed,
			| Outcome::Skipped => &mut self.skipped,
			| Outcome::Stale => &mut self.stale,
		};

		*count = count.saturating_add(1);
		self
	}
}

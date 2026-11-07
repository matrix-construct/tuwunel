//! Read-only integrity checks over the database.
//!
//! A check walks the columns it cares about and yields a finding for every
//! defect it sees, writing nothing. A finding describes the database as it
//! stands, so a check run after a repair sees that repair's work. The migrations
//! are a caller like any other: they consume these streams and write what a
//! finding calls for, re-deciding anything a write may have changed.

pub mod columns;
pub mod referencedevents;
mod scan;
pub mod schema;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, fmt::Display};

use futures::{FutureExt, Stream, future::BoxFuture};
use tuwunel_core::{Err, Result, implement, utils::TryReadyExt};

pub(crate) use self::scan::ScanExt;
use crate::Services;

type Walk = for<'a> fn(&'a Services, usize) -> BoxFuture<'a, Result<Summary>>;

/// Every check, in the order a whole run takes them.
static CHECKS: &[&Check] =
	&[&schema::CHECK, &columns::CHECK, &columns::UNUSED, &referencedevents::CHECK];

/// A finding of any check.
///
/// Rendering it describes the one defect, while its kind names the class of
/// defect, the unit a run counts by.
pub trait Report: Display + Send {
	/// Names the class of defect.
	///
	/// A run counts its findings under this label, so every finding of one class
	/// answers with the same text.
	fn kind(&self) -> &'static str;
}

/// One registered check.
///
/// The registry is how a caller reaches a check by name, as the admin suite does.
/// A caller that wants the findings themselves takes the typed stream of the
/// check's own module.
#[derive(Clone, Copy)]
pub struct Check {
	/// The name a caller selects it by.
	pub name: &'static str,

	/// One line saying what it verifies.
	pub about: &'static str,

	/// Whether it needs a database nothing else is writing.
	///
	/// A check comparing columns reads them at different moments, so on a serving
	/// database a concurrent write would read as a defect.
	pub quiescent: bool,

	walk: Walk,
}

/// What one run of a check found.
///
/// Counting holds no finding in memory, so a run over a badly damaged database
/// stays bounded by its sample limit.
#[derive(Default)]
pub struct Summary {
	/// Findings of each kind.
	pub counts: BTreeMap<&'static str, u64>,

	/// The first findings rendered, up to the limit of the run.
	pub samples: Vec<String>,
}

/// Finds the check registered under a name.
///
/// Names are unique, and `None` means no check has this one.
#[must_use]
pub fn find(name: &str) -> Option<&'static Check> { checks().find(|check| check.name == name) }

/// Iterates over every registered check.
///
/// The order is the one a whole run takes, cheapest and most fundamental first.
pub fn checks() -> impl Iterator<Item = &'static Check> + Clone + Send { CHECKS.iter().copied() }

/// Runs this check to the end, keeping the first `limit` findings rendered.
///
/// A quiescent check refuses a writable database, because everything it would
/// report could be a concurrent write. The first error ends the run, whatever it
/// found by then.
#[implement(Check)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		check = %self.name,
	),
)]
pub async fn run(&self, services: &Services, limit: usize) -> Result<Summary> {
	if !self.runnable(services) {
		return Err!("{} needs a database opened read-only", self.name);
	}

	(self.walk)(services, limit).await
}

/// Whether this check can give a true answer on this database.
///
/// Only a quiescent check on a writable database cannot.
#[implement(Check)]
#[must_use]
pub fn runnable(&self, services: &Services) -> bool {
	!self.quiescent || services.db.is_read_only()
}

/// The number of findings of every kind.
///
/// This is zero exactly when the check found the database clean.
#[implement(Summary)]
#[must_use]
pub fn findings(&self) -> u64 {
	self.counts
		.values()
		.copied()
		.fold(0, u64::saturating_add)
}

fn tally<'a, S, F>(findings: S, limit: usize) -> BoxFuture<'a, Result<Summary>>
where
	S: Stream<Item = Result<F>> + Send + 'a,
	F: Report + 'a,
{
	findings
		.ready_try_fold(Summary::default(), move |summary, finding| {
			Ok(summary.record(&finding, limit))
		})
		.boxed() // type-erasure boundary
}

#[implement(Summary)]
fn record(mut self, finding: &impl Report, limit: usize) -> Self {
	if self.samples.len() < limit {
		self.samples.push(finding.to_string());
	}

	let count = self.counts.entry(finding.kind()).or_default();

	*count = count.saturating_add(1);
	self
}

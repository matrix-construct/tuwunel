//! Progress of a long startup phase, readable while it runs.
//!
//! Whatever is doing the work names the phase it is in and counts items
//! through it; a ticker elsewhere decides how often to report. Nothing here
//! writes to a log or to the service manager.

use std::{
	sync::{
		Mutex, MutexGuard, PoisonError,
		atomic::{AtomicU64, Ordering},
	},
	time::Instant,
};

use smallstr::SmallString;

use crate::{format_small_string, implement, utils::time::pretty};

/// Inline budget for one formatted progress line.
type Line = SmallString<[u8; 128]>;

/// Inline budget for one part of a progress line.
type Part = SmallString<[u8; 48]>;

/// A named unit of long-running work, reported while it runs.
///
/// The publisher names a phase, optionally counts items through it, and ends
/// it. A reader takes whatever phase is current rather than waiting for one to
/// finish, so a phase that counts nothing still reports how long it has been
/// running. Only the position is lock-free, which is what lets a scan count
/// every row.
#[derive(Default)]
pub struct Progress {
	/// The phase in flight, absent before the first begins and after the last
	/// ends.
	phase: Mutex<Option<Phase>>,

	/// Items the phase in flight has finished.
	///
	/// The reset happens under the phase lock, so a report never pairs one
	/// phase's name with another's count. The increment takes no lock, so a
	/// report may trail the true count by a few items.
	position: AtomicU64,
}

/// One phase, from the moment it is named until the next one replaces it.
#[derive(Clone, Copy)]
struct Phase {
	/// What the publisher called this phase.
	step: &'static str,

	/// A narrower part of the step, when the publisher named one.
	pass: Option<&'static str>,

	/// When the step was named.
	began: Instant,

	/// Items the step expects to finish, when it can say exactly.
	total: Option<u64>,
}

/// Names the phase now in flight.
///
/// Any pass and any count the previous phase left are cleared, so a step that
/// counts nothing cannot inherit a number from the step before it. A name
/// reaches a service manager that delimits its own protocol by newline, so it
/// must not contain one.
#[implement(Progress)]
pub fn begin(&self, step: &'static str) {
	let mut phase = self.lock();

	self.position.store(0, Ordering::Relaxed);
	*phase = Some(Phase {
		step,
		pass: None,
		began: Instant::now(),
		total: None,
	});
}

/// Names a narrower part of the phase in flight.
///
/// The count resets with the pass, because the items a pass walks are its own.
/// A call arriving before any phase began is ignored.
#[implement(Progress)]
pub fn enter(&self, pass: &'static str) {
	let mut phase = self.lock();

	let Some(phase) = phase.as_mut() else {
		return;
	};

	self.position.store(0, Ordering::Relaxed);
	*phase = Phase { pass: Some(pass), total: None, ..*phase };
}

/// Records how many items the phase in flight expects to finish.
///
/// Only a step that can count its work exactly says so, since a report shows
/// the position against this total as if it were reached. Every other step
/// reports a bare position rather than a proportion of an estimate.
#[implement(Progress)]
pub fn expect_total(&self, total: u64) {
	let mut phase = self.lock();

	if let Some(phase) = phase.as_mut() {
		phase.total = Some(total);
	}
}

/// Counts one more item finished by the phase in flight.
///
/// The increment takes no lock, so a scan can afford to call it per row. A
/// phase change resets the count, so no report carries one phase's position
/// into another.
#[implement(Progress)]
#[inline]
pub fn advance(&self) { self.position.fetch_add(1, Ordering::Relaxed); }

/// Ends the phase in flight, leaving nothing to report.
///
/// A reader between the last phase and the next sees no phase at all, rather
/// than a finished one whose elapsed time keeps climbing.
#[implement(Progress)]
pub fn end(&self) {
	let mut phase = self.lock();

	*phase = None;
}

/// Formats the phase in flight, or nothing when there is none.
///
/// The name, the position and the elapsed time are read under one lock, so
/// they describe the same phase. A step that named an expected total reports
/// its position against that total; every other one reports a bare position,
/// or only its elapsed time when it counts nothing.
#[implement(Progress)]
pub fn report(&self) -> Option<Line> {
	let phase = self.lock();

	let Phase { step, pass, began, total } = (*phase)?;
	let position = self.position.load(Ordering::Relaxed);

	drop(phase);

	let pass: Part = pass.map_or_else(Part::new, |pass| format_small_string!(" / {pass}"));
	let counted: Part = match total {
		| None if position == 0 => Part::new(),
		| None => format_small_string!("{position} done, "),
		| Some(total) => format_small_string!("{position} of {total}, "),
	};

	let elapsed = pretty(began.elapsed());

	Some(format_small_string!("{step}{pass}, {counted}{elapsed}"))
}

/// Takes the phase lock, adopting a poisoned one.
///
/// A poisoned lock means a publisher panicked mid-update, which leaves the
/// phase merely stale. Refusing to report at all would be the worse failure.
#[implement(Progress)]
#[inline]
fn lock(&self) -> MutexGuard<'_, Option<Phase>> {
	self.phase
		.lock()
		.unwrap_or_else(PoisonError::into_inner)
}

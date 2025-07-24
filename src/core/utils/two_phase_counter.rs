//! Two-Phase Counter.

use std::{
	collections::VecDeque,
	fmt::Debug,
	ops::Deref,
	sync::{Arc, RwLock},
};

use crate::{Result, checked, is_equal_to};

/// Two-Phase Counter.
///
/// This device solves the problem of a One-Phase Counter (or just a counter)
/// which is incremented to provide unique sequence numbers (or index numbers)
/// fundamental to server operation. For example, let's say a new Matrix Pdu
/// is received: the counter is incremented and its value becomes the PduId
/// used as a key for the Pdu value when writing to the database.
///
/// Problem: With a single counter shared by both writers and readers, pending
/// writes might still be in-flight and not visible to readers after the writer
/// incremented it. For example, client-sync sees the counter at a certain
/// value, but that value has no Pdu found because its write has not been
/// completed with global visibility. Client-sync will then move on to the next
/// counter value having missed the data from the current one.
#[derive(Debug)]
pub struct Counter<F: Fn(u64) -> Result + Sync> {
	/// Self is intended to be Arc<Counter> with inner state mutable via Lock.
	inner: RwLock<State<F>>,
}

/// Inner protected state for Two-Phase Counter.
#[derive(Debug)]
pub struct State<F: Fn(u64) -> Result + Sync> {
	/// Monotonic counter. The next sequence number is drawn by adding one to
	/// this value. That number will be persisted and added to `pending`.
	dispatched: u64,

	/// Callback to persist the next sequence number drawn from `dispatched`.
	/// This prevents pending numbers from being reused after server restart.
	commit: F,

	/// List of pending sequence numbers. One less than the minimum value in
	/// this list is the "retirement" sequence number where all writes have
	/// completed and all reads are globally visible.
	pending: VecDeque<u64>,
}

#[derive(Debug)]
pub struct Permit<F: Fn(u64) -> Result + Sync> {
	/// Link back to the shared-state.
	state: Arc<Counter<F>>,

	/// The retirement value computed as a courtesy when this permit was
	/// created.
	retired: u64,

	/// Sequence number of this permit.
	id: u64,
}

impl<F: Fn(u64) -> Result + Sync> Counter<F> {
	/// Construct a new Two-Phase counter state. The value of `init` is
	/// considered retired, and the next sequence number dispatched will be one
	/// greater.
	pub fn new(init: u64, commit: F) -> Arc<Self> {
		Arc::new(Self { inner: State::new(init, commit).into() })
	}

	/// Obtain a sequence number to conduct write operations for the scope.
	pub fn next(self: &Arc<Self>) -> Result<Permit<F>> {
		let (retired, id) = self.inner.write()?.dispatch()?;

		Ok(Permit::<F> { state: self.clone(), retired, id })
	}

	/// Load the highest sequence number safe for reading, also known as the
	/// retirement value with writes "globally visible."
	#[inline]
	pub fn current(&self) -> u64 {
		self.inner
			.read()
			.expect("locked for reading")
			.retired()
	}
}

impl<F: Fn(u64) -> Result + Sync> State<F> {
	/// Create new state, starting from `init`. The next sequence number
	/// dispatched will be one greater than `init`.
	fn new(dispatched: u64, commit: F) -> Self {
		Self {
			dispatched,
			commit,
			pending: VecDeque::new(),
		}
	}

	/// Dispatch the next sequence number as pending. The retired value is
	/// calculated as a courtesy while the state is under lock.
	fn dispatch(&mut self) -> Result<(u64, u64)> {
		let retired = self.retired();
		let prev = self.dispatched;

		self.dispatched = checked!(prev + 1)?;
		(self.commit)(self.dispatched)?;
		debug_assert!(
			!self.check_pending(self.dispatched),
			"sequence number cannot already be pending",
		);

		self.pending.push_back(self.dispatched);
		Ok((retired, self.dispatched))
	}

	/// Retire the sequence number `id`.
	fn retire(&mut self, id: u64) {
		debug_assert!(self.check_pending(id), "sequence number must be currently pending",);

		let index = self
			.pending_index(id)
			.expect("sequence number must be found as pending");

		let removed = self
			.pending
			.remove(index)
			.expect("sequence number at index must be removed");

		debug_assert!(removed == id, "sequence number removed must match id");
	}

	/// Calculate the retired sequence number, one less than the lowest pending
	/// sequence number. If nothing is pending the value of `dispatched` has
	/// been previously retired and is returned.
	fn retired(&self) -> u64 {
		debug_assert!(
			self.pending.iter().is_sorted(),
			"Pending values should be naturally sorted"
		);

		self.pending
			.front()
			.map(|val| val.saturating_sub(1))
			.unwrap_or(self.dispatched)
	}

	/// Get the position of `id` in the pending list.
	fn pending_index(&self, id: u64) -> Option<usize> {
		debug_assert!(
			self.pending.iter().is_sorted(),
			"Pending values should be naturally sorted"
		);

		self.pending.binary_search(&id).ok()
	}

	/// Check for `id` in the pending list sequentially (for debug and assertion
	/// purposes only)
	fn check_pending(&self, id: u64) -> bool { self.pending.iter().any(is_equal_to!(&id)) }
}

impl<F: Fn(u64) -> Result + Sync> Permit<F> {
	/// Access the retired sequence number sampled at this permit's creation.
	/// This may be outdated prior to access. Obtained as a courtesy under lock.
	#[inline]
	#[must_use]
	pub fn retired(&self) -> &u64 { &self.retired }

	/// Access the sequence number obtained by this permit; a unique value
	#[inline]
	#[must_use]
	pub fn id(&self) -> &u64 { &self.id }
}

impl<F: Fn(u64) -> Result + Sync> Deref for Permit<F> {
	type Target = u64;

	#[inline]
	fn deref(&self) -> &Self::Target { self.id() }
}

impl<F: Fn(u64) -> Result + Sync> Drop for Permit<F> {
	fn drop(&mut self) {
		self.state
			.inner
			.write()
			.expect("locked for writing")
			.retire(self.id);
	}
}

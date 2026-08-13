use std::cmp::Ordering;

use ruma::events::StateEventType;
use smallstr::SmallString;

/// Composite key identifying one state event slot.
///
/// The event type is compared before the state-key string. This layout is used
/// as the key for in-memory state maps.
pub type TypeStateKey = (StateEventType, StateKey);

/// Inline-backed string used for Matrix state keys.
///
/// The inline budget lets short keys remain inline, while longer keys spill to
/// heap storage.
pub type StateKey = SmallString<[u8; INLINE_SIZE]>;

const INLINE_SIZE: usize = 48;

/// Compares state keys in ascending event-type and state-key order.
///
/// Event type is the primary key and the state-key string breaks ties. The
/// ordering matches the natural tuple order of `TypeStateKey`.
#[inline]
#[must_use]
pub fn cmp(a: &TypeStateKey, b: &TypeStateKey) -> Ordering { a.0.cmp(&b.0).then(a.1.cmp(&b.1)) }

/// Compares state keys in descending event-type and state-key order.
///
/// Both components are reversed together, producing the inverse of `cmp`. It is
/// suitable for descending sorts over the same key domain.
#[inline]
#[must_use]
pub fn rcmp(a: &TypeStateKey, b: &TypeStateKey) -> Ordering { b.0.cmp(&a.0).then(b.1.cmp(&a.1)) }

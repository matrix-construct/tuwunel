#![cfg(test)]

use std::num::NonZeroUsize;

use ruma::UInt;

use super::{effective_cap, usize_from_ruma_bounded};

#[test]
fn effective_cap_clamps() {
	assert_eq!(effective_cap(Some(nz(4)), 0), 4);
	assert_eq!(effective_cap(None, 0), usize::MAX);

	assert_eq!(effective_cap(Some(nz(4)), 2), 2, "config tightens the opts cap");
	assert_eq!(effective_cap(Some(nz(2)), 4), 2, "config never widens the opts cap");
	assert_eq!(effective_cap(None, 3), 3, "config bounds an unbounded profile");
}

#[test]
fn usize_from_ruma_bounded_clamps() {
	assert_eq!(usize_from_ruma_bounded(UInt::MIN, 7, 20), 0);
	assert_eq!(usize_from_ruma_bounded(UInt::from(20_u32), 7, 20), 20);
	assert_eq!(usize_from_ruma_bounded(UInt::from(21_u32), 7, 20), 20);
	assert_eq!(usize_from_ruma_bounded(UInt::MAX, 7, 20), 20);
}

#[cfg(target_pointer_width = "32")]
#[test]
fn usize_from_ruma_bounded_uses_fallback_on_overflow() {
	assert_eq!(usize_from_ruma_bounded(UInt::MAX, 7, 20), 7);
	assert_eq!(usize_from_ruma_bounded(UInt::MAX, 30, 20), 20);
}

fn nz(value: usize) -> NonZeroUsize { NonZeroUsize::new(value).expect("value must be nonzero") }

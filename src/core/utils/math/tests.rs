#![cfg(test)]

use std::num::NonZeroUsize;

use super::effective_cap;

#[test]
fn effective_cap_clamps() {
	assert_eq!(effective_cap(Some(nz(4)), 0), 4);
	assert_eq!(effective_cap(None, 0), usize::MAX);

	assert_eq!(effective_cap(Some(nz(4)), 2), 2, "config tightens the opts cap");
	assert_eq!(effective_cap(Some(nz(2)), 4), 2, "config never widens the opts cap");
	assert_eq!(effective_cap(None, 3), 3, "config bounds an unbounded profile");
}

fn nz(value: usize) -> NonZeroUsize { NonZeroUsize::new(value).expect("value must be nonzero") }

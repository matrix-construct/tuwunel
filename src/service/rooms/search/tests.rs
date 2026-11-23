//! Hit list intersection behind multi-word room searches.

use tuwunel_matrix::PduCount;

use super::{intersect_hits, query_words};
use crate::rooms::timeline::{PduId, RawPduId};

/// The hits are those in every list, newest first, whichever list is shortest.
#[test]
fn hits_are_in_every_list_newest_first() {
	let lists = vec![ids(&[9, 7, 5, 3, 1]), ids(&[7, 3]), ids(&[9, 7, 3, 2])];

	assert_eq!(intersect_hits(lists), ids(&[7, 3]));
	assert_eq!(intersect_hits(vec![ids(&[2, 1]); 2]), ids(&[2, 1]));
	assert_eq!(intersect_hits(vec![ids(&[2, 1])]), ids(&[2, 1]));
	assert!(intersect_hits(vec![ids(&[2]), Vec::new()]).is_empty());
	assert!(intersect_hits(Vec::new()).is_empty());
}

/// A repeated word, in any case, is read once, where it first appears.
#[test]
fn query_words_are_distinct_in_first_seen_order() {
	let words: Vec<_> = query_words("Go far to go, fast to GO!").collect();

	assert_eq!(words, ["go", "far", "to", "fast"]);
	assert_eq!(query_words("?! ...").count(), 0);
}

fn ids(counts: &[u64]) -> Vec<RawPduId> { counts.iter().copied().map(id).collect() }

fn id(count: u64) -> RawPduId {
	PduId {
		shortroomid: 7,
		count: PduCount::Normal(count),
	}
	.into()
}

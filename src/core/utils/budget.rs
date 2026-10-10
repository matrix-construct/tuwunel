//! Size budgets over ordered items.
//!
//! A budget keeps each item that still fits within a limit together with the
//! items kept before it, without reordering anything, so the same items always
//! yield the same selection.

/// Marks each item, in order, `Ok` while its size fits within `limit` together
/// with the items kept before it, and `Err` otherwise.
///
/// An item that does not fit is skipped rather than ending the walk, so a later
/// smaller item may still be kept. The selection depends only on the sizes and
/// their order.
pub fn budget<Item, Iter, Size>(
	items: Iter,
	limit: usize,
	size: Size,
) -> impl Iterator<Item = Result<Item, Item>>
where
	Iter: IntoIterator<Item = Item>,
	Size: Fn(&Item) -> usize,
{
	items
		.into_iter()
		.scan(0_usize, move |total, item| {
			let next = total.saturating_add(size(&item));
			let fits = next <= limit;

			if fits {
				*total = next;
			}

			Some(if fits { Ok(item) } else { Err(item) })
		})
}

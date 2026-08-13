use ::arrayvec::ArrayVec;

/// Adds fluent slice extension to fixed-capacity vectors.
///
/// Elements are copied into the vector's inline storage. The returned mutable
/// reference permits continued method chaining.
pub trait ArrayVecExt<T> {
	/// Appends every element from `other` and returns the vector.
	///
	/// The operation copies the slice without allocating fallback storage. On
	/// success, each slice element is appended in order.
	///
	/// # Panics
	///
	/// Panics when the remaining capacity cannot hold the entire slice.
	fn extend_from_slice(&mut self, other: &[T]) -> &mut Self;
}

impl<T: Copy, const CAP: usize> ArrayVecExt<T> for ArrayVec<T, CAP> {
	#[inline]
	fn extend_from_slice(&mut self, other: &[T]) -> &mut Self {
		self.try_extend_from_slice(other)
			.expect("Insufficient buffer capacity to extend from slice");

		self
	}
}

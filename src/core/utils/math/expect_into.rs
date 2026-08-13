/// Converts values through [`TryFrom`] and expects conversion to succeed.
///
/// The destination type is selected by the caller or inferred from context.
/// Conversion failures are treated as violated program invariants.
pub trait ExpectInto {
	/// Converts the value into `Dst` and returns the successful result.
	///
	/// The implementation delegates to the shared checked conversion helper. It
	/// discards the conversion error after using a fixed expectation message.
	///
	/// # Panics
	///
	/// Panics when `Dst::try_from` rejects the source value.
	#[inline]
	#[must_use]
	fn expect_into<Dst: TryFrom<Self>>(self) -> Dst
	where
		Self: Sized,
	{
		super::expect_into::<Dst, Self>(self)
	}
}

impl<T> ExpectInto for T {}

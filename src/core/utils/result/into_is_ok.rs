use super::Result;

pub trait IntoIsOk<T, E> {
	fn into_is_ok(self) -> bool;
}

impl<T, E> IntoIsOk<T, E> for Result<T, E> {
	#[inline]
	fn into_is_ok(self) -> bool {
		self.is_ok()
	}
}

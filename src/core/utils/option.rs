use futures::future::OptionFuture;

pub trait OptionExt<T> {
	fn map_async<O: Future, F: FnOnce(T) -> O>(self, f: F) -> OptionFuture<O>;
}

impl<T> OptionExt<T> for Option<T> {
	fn map_async<O: Future, F: FnOnce(T) -> O>(self, f: F) -> OptionFuture<O> {
		OptionFuture::<_>::from(self.map(f))
	}
}

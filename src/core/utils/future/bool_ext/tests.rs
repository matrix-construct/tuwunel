use std::{
	iter::{Empty, empty},
	mem::take,
	task::Poll,
};

use futures::{
	FutureExt,
	future::{Ready, poll_fn, ready},
};

use super::{BoolExt, and, or};

#[tokio::test]
async fn true_elides_later_poll() {
	let later = poll_fn(|_| -> Poll<bool> { panic!("later future was polled") });

	assert!(ready(true).or(later).await);
}

#[tokio::test]
async fn false_or_false() {
	assert!(!ready(false).or(ready(false)).await);
}

#[tokio::test]
async fn pending_then_true() {
	let mut pending = true;
	let input = poll_fn(move |cx| match take(&mut pending) {
		| false => Poll::Ready(true),
		| true => {
			cx.waker().wake_by_ref();
			Poll::Pending
		},
	});

	assert!(input.or(ready(false)).await);
}

#[tokio::test]
async fn empty_iterator() {
	let inputs: Empty<Ready<bool>> = empty();

	assert!(!or(inputs).await);
	assert!(and(empty().map(ready)).await);
}

#[test]
fn iterator_or_decides_past_pending() {
	let inputs = (0..31).map(|i| {
		poll_fn(move |_| match i {
			| 0 => Poll::Pending,
			| _ => Poll::Ready(i == 30),
		})
	});

	assert_eq!(or(inputs).now_or_never(), Some(true));
}

#[test]
fn iterator_and_decides_past_pending() {
	let inputs = (0..31).map(|i| {
		poll_fn(move |_| match i {
			| 0 => Poll::Pending,
			| _ => Poll::Ready(i != 30),
		})
	});

	assert_eq!(and(inputs).now_or_never(), Some(false));
}

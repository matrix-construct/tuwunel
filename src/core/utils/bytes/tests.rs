use super::prefix_successor;

#[test]
fn successor_skips_a_complete_prefix() {
	for (prefix, expected) in [
		(vec![1], Some(vec![2])),
		(vec![1, 2], Some(vec![1, 3])),
		(vec![1, 2, 255, 255], Some(vec![1, 3])),
		(vec![254, 255], Some(vec![255])),
		(vec![255, 255], None),
		(vec![], None),
	] {
		let successor = prefix_successor(prefix.clone());

		assert_eq!(successor, expected);

		let Some(successor) = successor else {
			continue;
		};

		for suffix in [b"".as_slice(), &[0], &[255], &[255, 255]] {
			let key: Vec<_> = prefix.iter().chain(suffix).copied().collect();

			assert!(key < successor);
		}
	}
}

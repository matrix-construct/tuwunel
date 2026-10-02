use super::{NativeChoice, should_serve_native};

#[test]
fn native_decision_truth_table() {
	let base = NativeChoice {
		native_enabled: true,
		has_default_idp: false,
	};

	assert!(!should_serve_native(NativeChoice { native_enabled: false, ..base }));
	assert!(!should_serve_native(NativeChoice {
		native_enabled: false,
		has_default_idp: true
	}));

	assert!(should_serve_native(base));

	assert!(!should_serve_native(NativeChoice { has_default_idp: true, ..base }));
}

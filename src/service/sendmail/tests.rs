use super::Address;

#[test]
fn address_requires_local_part_and_domain() {
	for valid in ["user@example.com", "first.last+tag@sub.example.org"] {
		let address = Address::try_from(valid.to_owned()).expect("a well-formed address parses");
		let shown: &str = address.as_ref();

		assert_eq!(shown, valid, "the address keeps the string as given");
	}

	for malformed in ["", "no-at-sign", "@example.com", "user@"] {
		Address::try_from(malformed.to_owned()).expect_err("a malformed address is refused");
	}
}

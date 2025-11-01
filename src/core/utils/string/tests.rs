#![cfg(test)]

#[test]
fn common_prefix() {
	let input = ["conduwuit", "conduit", "construct"];
	let output = super::common_prefix(&input);
	assert_eq!(output, "con");
}

#[test]
fn common_prefix_empty() {
	let input = ["abcdefg", "hijklmn", "opqrstu"];
	let output = super::common_prefix(&input);
	assert_eq!(output, "");
}

#[test]
fn common_prefix_none() {
	let input: [&str; 0] = [];
	let output = super::common_prefix(&input);
	assert_eq!(output, "");
}

#[test]
fn camel_to_snake_case_0() {
	let res = super::camel_to_snake_string("CamelToSnakeCase");
	assert_eq!(res, "camel_to_snake_case");
}

#[test]
fn camel_to_snake_case_1() {
	let res = super::camel_to_snake_string("CAmelTOSnakeCase");
	assert_eq!(res, "camel_tosnake_case");
}

#[test]
fn unquote() {
	use super::Unquote;

	assert_eq!("\"foo\"".unquote(), Some("foo"));
	assert_eq!("\"foo".unquote(), None);
	assert_eq!("foo".unquote(), None);
}

#[test]
fn unquote_infallible() {
	use super::Unquote;

	assert_eq!("\"foo\"".unquote_infallible(), "foo");
	assert_eq!("\"foo".unquote_infallible(), "\"foo");
	assert_eq!("foo".unquote_infallible(), "foo");
}

#[test]
fn between() {
	use super::Between;

	assert_eq!("\"foo\"".between(("\"", "\"")), Some("foo"));
	assert_eq!("\"foo".between(("\"", "\"")), None);
	assert_eq!("foo".between(("\"", "\"")), None);
}

#[test]
fn between_infallible() {
	use super::Between;

	assert_eq!("\"foo\"".between_infallible(("\"", "\"")), "foo");
	assert_eq!("\"foo".between_infallible(("\"", "\"")), "\"foo");
	assert_eq!("foo".between_infallible(("\"", "\"")), "foo");
}

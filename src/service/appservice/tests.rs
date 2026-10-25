use super::socket_path;

#[test]
fn socket_urls() {
	for (url, expected) in [
		("unix:///run/a.sock", Some("/run/a.sock")),
		("unix:/run/a.sock", Some("/run/a.sock")),
		("UNIX:///run/a.sock", Some("/run/a.sock")),
		("unix:///run/my%20dir/a.sock", Some("/run/my%20dir/a.sock")),
		("http://127.0.0.1:1234", None),
	] {
		assert_eq!(socket_path(url).unwrap(), expected, "{url}");
	}

	for url in ["unix://run/a.sock", "unix:run/a.sock"] {
		assert!(socket_path(url).is_err(), "{url}");
	}
}

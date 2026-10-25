fn main() {
	// The MSC4521 wire types are declared with ruma's `#[request]`/`#[response]`
	// macros, which read this cfg from the destination crate.
	println!("cargo::rustc-cfg=ruma_unstable_exhaustive_types");
	println!("cargo::rerun-if-changed=build.rs");
}

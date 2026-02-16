use std::{process::Command, str};

use proc_macro::TokenStream;
use quote::quote;

pub(super) fn semantic(_args: TokenStream) -> TokenStream {
	static ARGS: &[&str] = &["describe", "--tags", "--abbrev=1"];

	let output = git(ARGS);
	let output = output
		.strip_prefix('v')
		.map(ToOwned::to_owned)
		.unwrap_or(output);

	let output = output
		.rsplit_once('-')
		.map(|(s, _)| s)
		.map(ToOwned::to_owned)
		.unwrap_or(output);

	let ret = quote! {
		static GIT_SEMANTIC: &'static str = #output;
	};

	ret.into()
}

pub(super) fn commit(_args: TokenStream) -> TokenStream {
	static ARGS: &[&str] = &["describe", "--always", "--dirty", "--abbrev=10"];

	let output = git(ARGS);
	let ret = quote! {
		static GIT_COMMIT: &'static str = #output;
	};

	ret.into()
}

fn git(args: &[&str]) -> String {
	Command::new("git")
		.args(args)
		.output()
		.map(|output| {
			str::from_utf8(&output.stdout)
				.map(str::trim)
				.map(String::from)
				.ok()
		})
		.ok()
		.flatten()
		.unwrap_or_default()
}

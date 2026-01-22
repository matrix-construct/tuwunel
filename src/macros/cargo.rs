use std::{fs::read_to_string, path::PathBuf};

use proc_macro::{Span, TokenStream};
use quote::quote;
use syn::{Error, ItemConst, Meta};

use crate::{Result, utils};

pub(super) fn manifest(item: ItemConst, args: &[Meta]) -> Result<TokenStream> {
	let member = utils::get_named_string(args, "crate");
	let path = manifest_path(member.as_deref())?;
	let manifest = read_to_string(&path).unwrap_or_default();
	let val = manifest.as_str();
	let name = item.ident;
	let ret = quote! {
		const #name: &'static str = #val;
	};

	Ok(ret.into())
}

fn manifest_path(member: Option<&str>) -> Result<PathBuf> {
	let Some(path) = option_env!("CARGO_MANIFEST_DIR") else {
		return Err(Error::new(
			Span::call_site().into(),
			"missing CARGO_MANIFEST_DIR in environment",
		));
	};

	let mut path: PathBuf = path.into();

	// tuwunel/src/macros/ -> tuwunel/src/
	path.pop();

	if let Some(member) = member {
		// tuwunel/$member/Cargo.toml
		path.push(member);
	} else {
		// tuwunel/src/ -> tuwunel/
		path.pop();
	}

	path.push("Cargo.toml");

	Ok(path)
}

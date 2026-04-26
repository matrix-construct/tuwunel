use proc_macro::{Span, TokenStream};
use quote::quote;
use syn::{Error, ItemFn, Meta, Path};
use utils::get_named_generics;

use crate::{Result, utils};

pub(super) fn implement(item: ItemFn, args: &[Meta]) -> Result<TokenStream> {
	let generics = get_named_generics(args, "generics")?;
	let receiver = get_receiver(args)?;
	let params = get_named_generics(args, "params")?;
	let input = item;
	let out = quote! {
		impl #generics #receiver #params {
			#input
		}
	};

	Ok(out.into())
}

fn get_receiver(args: &[Meta]) -> Result<&Path> {
	let receiver = args.first().ok_or_else(|| {
		Error::new(Span::call_site().into(), "Missing required argument to receiver")
	})?;

	let Meta::Path(receiver) = receiver else {
		return Err(Error::new(
			Span::call_site().into(),
			"First argument is not path to receiver",
		));
	};

	Ok(receiver)
}

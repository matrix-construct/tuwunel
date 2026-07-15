use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Error, FnArg, ItemFn, Meta, ReturnType};

use crate::Result;

/// Split an `async fn` into a non-generic `#[inline(never)]` wrapper returning
/// a boxed future and a private `async` body carrying the real work. The
/// wrapper type-erases the future so its codegen stays in the defining crate
/// instead of monomorphizing into every caller. Attributes below this one ride
/// the body, so a `tracing` span still sees the parameter bindings it names;
/// give that span an explicit `name`, since the body is the hidden inner fn.
/// Doc comments stay on the wrapper. A lone declared lifetime bounds the boxed
/// future, else `'_`; unify borrows of `self` under one lifetime to make that
/// bound sound.
pub(super) fn async_noinline(item: ItemFn, _args: &[Meta]) -> Result<TokenStream> {
	let ItemFn { attrs, vis, sig, block } = item;

	if sig.asyncness.is_none() {
		return Err(Error::new(sig.ident.span(), "#[async_noinline] expects an async fn"));
	}

	let output = match &sig.output {
		| ReturnType::Default => quote!(()),
		| ReturnType::Type(_, ty) => quote!(#ty),
	};

	let mut lifetimes = sig.generics.lifetimes().map(|lt| &lt.lifetime);
	let boxed_lifetime = match (lifetimes.next(), lifetimes.next()) {
		| (Some(lifetime), None) => quote!(#lifetime),
		| _ => quote!('_),
	};

	let (impl_generics, _, where_clause) = sig.generics.split_for_impl();

	let (doc_attrs, body_attrs): (Vec<_>, Vec<_>) = attrs
		.iter()
		.partition(|attr| attr.path().is_ident("doc"));

	let name = &sig.ident;
	let inner_name = format_ident!("__{}", name);
	let has_receiver = sig
		.inputs
		.iter()
		.any(|arg| matches!(arg, FnArg::Receiver(_)));

	let (wrapper_inputs, call_args): (Vec<_>, Vec<Option<_>>) = sig
		.inputs
		.iter()
		.enumerate()
		.map(|(i, arg)| match arg {
			| FnArg::Receiver(receiver) => (quote!(#receiver), None),
			| FnArg::Typed(arg) => {
				let binding = format_ident!("__arg{}", i);
				let ty = &arg.ty;

				(quote!(#binding: #ty), Some(quote!(#binding)))
			},
		})
		.unzip();

	let call_args = call_args.into_iter().flatten();
	let self_prefix = has_receiver.then(|| quote!(self.));
	let call = quote!(#self_prefix #inner_name(#(#call_args),*));

	let inner_inputs = &sig.inputs;
	let inner_output = &sig.output;
	let out = quote! {
		#(#doc_attrs)*
		#[inline(never)]
		#[must_use]
		#vis fn #name #impl_generics (#(#wrapper_inputs),*)
			-> ::std::pin::Pin<::std::boxed::Box<
				dyn ::std::future::Future<Output = #output> + Send + #boxed_lifetime
			>>
		#where_clause
		{
			::std::boxed::Box::pin(#call)
		}

		#(#body_attrs)*
		async fn #inner_name #impl_generics (#inner_inputs) #inner_output
		#where_clause
		#block
	};

	Ok(out.into())
}

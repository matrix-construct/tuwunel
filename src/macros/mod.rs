mod admin;
mod cargo;
mod config;
mod debug;
mod git;
mod implement;
mod rustc;
mod utils;

use proc_macro::TokenStream;
use syn::{
	Error, Item, ItemConst, ItemEnum, ItemFn, ItemStruct, Meta,
	parse::{Parse, Parser},
	parse_macro_input,
};

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[proc_macro_attribute]
pub fn admin_command(args: TokenStream, input: TokenStream) -> TokenStream {
	attribute_macro::<ItemFn, _>(args, input, admin::command)
}

#[proc_macro_attribute]
pub fn admin_command_dispatch(args: TokenStream, input: TokenStream) -> TokenStream {
	attribute_macro::<ItemEnum, _>(args, input, admin::command_dispatch)
}

#[proc_macro_attribute]
pub fn cargo_manifest(args: TokenStream, input: TokenStream) -> TokenStream {
	attribute_macro::<ItemConst, _>(args, input, cargo::manifest)
}

#[proc_macro_attribute]
pub fn recursion_depth(args: TokenStream, input: TokenStream) -> TokenStream {
	attribute_macro::<Item, _>(args, input, debug::recursion_depth)
}

#[proc_macro]
pub fn rustc_flags_capture(args: TokenStream) -> TokenStream { rustc::flags_capture(args) }

#[proc_macro]
pub fn rustc_version(args: TokenStream) -> TokenStream { rustc::version(args) }

#[proc_macro_attribute]
pub fn implement(args: TokenStream, input: TokenStream) -> TokenStream {
	attribute_macro::<ItemFn, _>(args, input, implement::implement)
}

#[proc_macro_attribute]
pub fn config_example_generator(args: TokenStream, input: TokenStream) -> TokenStream {
	attribute_macro::<ItemStruct, _>(args, input, config::example_generator)
}

#[proc_macro]
pub fn git_semantic(args: TokenStream) -> TokenStream { git::semantic(args) }

#[proc_macro]
pub fn git_commit(args: TokenStream) -> TokenStream { git::commit(args) }

fn attribute_macro<I, F>(args: TokenStream, input: TokenStream, func: F) -> TokenStream
where
	F: Fn(I, &[Meta]) -> Result<TokenStream>,
	I: Parse,
{
	let item = parse_macro_input!(input as I);
	syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated
		.parse(args)
		.map(|args| args.iter().cloned().collect::<Vec<_>>())
		.and_then(|ref args| func(item, args))
		.unwrap_or_else(|e| e.to_compile_error().into())
}

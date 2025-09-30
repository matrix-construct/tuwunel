#![type_length_limit = "163840"] //TODO: REDUCE ME
#![allow(clippy::toplevel_ref_arg)]

pub mod client;
pub mod router;
pub mod server;
mod utils;

pub(crate) use self::router::{Ruma, RumaResponse, State};

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}

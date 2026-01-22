#![type_length_limit = "262144"] //TODO: REDUCE ME
#![expect(clippy::toplevel_ref_arg)]

pub mod client;
pub mod router;
pub mod server;

use log as _;

pub(crate) use self::router::{Ruma, RumaResponse, State};

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}

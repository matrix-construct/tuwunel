#![expect(clippy::toplevel_ref_arg)]
#![allow(unused_features)] // 1.96.0-nightly 2026-03-07 bug

pub mod client;
pub mod oidc;
pub mod router;
pub mod server;

use log as _;

pub(crate) use self::router::{Ruma, RumaResponse, State};

tuwunel_core::mod_ctor! {}
tuwunel_core::mod_dtor! {}

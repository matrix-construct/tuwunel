//! State resolution and checks on incoming PDUs according to the [Matrix](https://matrix.org/) specification.
//!
//! When creating or receiving a PDU (or event), a server must check whether it
//! is valid and how it affects the room state. The purpose of this crate is to
//! provide functions that solve that.
//!
//! # Checks performed on receipt of a PDU
//!
//! This crate used with [ruma-signatures] should allow to perform all the
//! [necessary checks on receipt of a PDU].
//!
//! To respect the Matrix specification, the following functions should be
//! called for a PDU:
//!
//! 1. [`check_pdu_format()`] - The event should be dropped on error.
//! 2. [`ruma_signatures::verify_event()`] - The event should be dropped on
//!    error. The PDU should be redacted before checking the authorization rules
//!    if the result is `Verified::Signatures`.
//! 3. [`check_state_independent_auth_rules()`] - The event should be rejected
//!    on error.
//! 4. [`check_state_dependent_auth_rules()`] - This function must be called 3
//!    times:
//!     1. With the `auth_events` for the state, the event should be rejected on
//!        error.
//!     2. With the state before the event, the event should be rejected on
//!        error.
//!     3. With the current state of the room, the event should be "soft failed"
//!        on error.
//!
//! # Room State Resolution
//!
//! Because of the distributed nature of Matrix, homeservers might not receive
//! all events in the same order, which might cause the room state to diverge
//! temporarily between homeservers. The purpose of [state resolution] is to
//! make sure that all homeservers arrive at the same room state given the same
//! events.
//!
//! The [`resolve()`] function allows to do that. It takes an iterator of state
//! maps and applies the proper state resolution algorithm for the current room
//! version to output the map of events in the current room state.
//!
//! # Event helper types
//!
//! The types from [ruma-events] use strict deserialization rules according to
//! their definition in the specification, which means that they also validate
//! fields that are not checked when receiving a PDU. Since it is not
//! appropriate for servers to reject an event that passes those checks, this
//! crate provides helper types in the [`events`] module, built around the
//! [`Event`] trait, to deserialize lazily a handful of fields from the most
//! common state events. Since these are the same types used for checking the
//! authorization rules, they are guaranteed to not perform extra validation on
//! unvalidated fields.
//!
//! The types from ruma-events are still appropriate to be used to create a new
//! event, or to validate an event received from a client.
//!
//! [ruma-signatures]: https://crates.io/crates/ruma-signatures
//! [necessary checks on receipt of a PDU]: https://spec.matrix.org/latest/server-server-api/#checks-performed-on-receipt-of-a-pdu
//! [ruma-events]: https://crates.io/crates/ruma-events

mod event_auth;
pub mod events;
mod fetch_state;
mod resolve;
#[cfg(test)]
mod test_utils;
pub mod topological_sort;

use tuwunel_core::matrix::TypeStateKey;

use self::{event_auth::check_state_dependent_auth_rules, fetch_state::FetchStateExt};
pub use self::{
	event_auth::{AuthTypes, auth_check, auth_types_for_event},
	resolve::{AuthSet, ConflictMap, StateMap, resolve},
	topological_sort::topological_sort,
};

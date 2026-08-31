//! cuw-daemon internals, exposed as a library so integration tests can drive the
//! router and the poll-update logic without a real socket or network (M2). The
//! `cuw-daemon` binary is a thin wrapper over these modules.

pub mod auth;
pub mod config;
pub mod http;
pub mod poll;
pub mod registry;
pub mod session;
pub mod startup;
pub mod state;

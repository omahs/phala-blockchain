//! This crate provides some instrumentation to write sidevm programs. It is built on top of the
//! Sidevm ocalls.

#![deny(missing_docs)]

pub use env::ocall_funcs_guest as ocall;
pub use res_id::ResourceId;
pub use sidevm_env as env;
pub use sidevm_logger as logger;
pub use sidevm_macro::main;

pub use env::spawn;
pub use env::tasks as task;

pub mod channel;
pub mod exec;
pub mod net;
pub mod time;

mod res_id;

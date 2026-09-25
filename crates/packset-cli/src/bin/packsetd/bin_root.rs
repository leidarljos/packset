//! `packsetd` as the `packset` crate's binary. The writer's modules sit at
//! this crate root, so the `crate::` paths inside them hold whichever
//! package builds them; the library root beside this file declares the
//! same modules for the examples and the tests.
#![allow(dead_code, unused_imports)]

pub mod cards;
pub mod context;
pub mod embed;
pub mod glob;
pub mod home;
pub mod http;
pub mod milli;
pub mod proposals;
mod run;
pub mod service;
pub mod store;
pub mod workspace;

pub use home::Home;
pub use service::Service;
pub use store::Store;

fn main() -> anyhow::Result<()> {
    run::run()
}

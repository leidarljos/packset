//! The loopback pack writer.
//!
//! One process owns `memory.lmdb` and every client speaks HTTP to it, so an
//! isolated harness home does not get a private store. Cards stay files
//! because a person edits them; atoms are a database because a program does.

#[allow(dead_code)]
#[path = "../../procfs.rs"]
mod procfs;

pub mod admit;
pub mod auth;
pub mod cards;
pub mod context;
pub mod embed;
pub mod glob;
pub mod home;
pub mod http;
pub mod journal;
pub mod milli;
pub mod proposals;
pub mod service;
pub mod store;
pub mod wire;
pub mod workspace;

pub use home::Home;
pub use service::Service;
pub use store::Store;

pub mod run;
pub use run::run;

#[cfg(test)]
#[allow(dead_code)]
mod test_support {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test_support/harness.rs"
    ));
}

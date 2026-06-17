#[rustfmt::skip]
#[path = "cbor.rs"]
pub mod cbor;

pub mod artifact;
pub mod git;
pub mod model;
pub mod operation;
pub mod protocol;
pub mod runtime;
pub mod status;
pub mod workspace;
pub mod workspace_ops;

pub use cbor::{Cbor, decode, encode};
pub use protocol::generated::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn version() -> &'static str {
    VERSION
}

#[cfg(test)]
#[rustfmt::skip]
#[path = "../protocol/corpus/rust/vectors.rs"]
mod protocol_corpus;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_package_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}

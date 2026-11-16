//! Test helpers: fixtures, and the SDK's in-memory server and runner.

pub use fronde_component::testing::{FakeServer, block_on};

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

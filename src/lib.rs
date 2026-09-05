/// The build this binary is. Read from `Cargo.toml` at compile time, so a client cannot report a
/// version it is not — which is the entire point of putting it on the wire.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod client;
pub mod hook;
pub mod identity;
pub mod hub;
pub mod relay;
pub mod server;
pub mod store;
pub mod update;
pub mod watch;

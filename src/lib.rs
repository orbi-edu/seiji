pub mod actions;
pub mod client;
pub mod config;
pub mod devices;
pub mod evidence;
pub mod exec;
pub mod forward;
pub mod http;
pub mod ios;
pub mod maestro;
pub mod mcp;
pub mod pool;
pub mod runs;
pub mod store;
pub mod tenant;
pub mod viewer;

pub(crate) const USER_AGENT: &str = concat!("seiji/", env!("CARGO_PKG_VERSION"));

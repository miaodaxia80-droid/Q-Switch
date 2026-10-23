//! Q Switch: Qoder provider configuration and a loopback protocol gateway.
pub mod config;
pub mod database;
#[cfg(feature = "desktop")]
mod desktop;
pub mod error;
pub mod provider;
pub mod proxy;
pub mod qoder_config;
#[cfg(feature = "desktop")]
pub use desktop::run;

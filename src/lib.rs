//! Hermes library — core types and functionality for integration tests.
//!
//! This library crate exposes internal types to integration tests while
//! keeping the main binary logic in `main.rs`.

pub mod agent;
pub mod config;
pub mod error;
pub mod session;
pub mod slack;
pub mod sync;
pub mod util;

// Re-export commonly used types for convenience
pub use error::{HermesError, Result};

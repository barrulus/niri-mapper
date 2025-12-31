//! Configuration parsing for niri-mapper
//!
//! This crate handles parsing KDL configuration files and generating
//! niri-compatible KDL keybind files.

mod error;
mod model;
mod parser;
mod generator;

pub use error::ConfigError;
pub use model::*;
pub use parser::parse_config;
pub use generator::generate_niri_keybinds;

use miette::Diagnostic;
use thiserror::Error;

#[derive(Error, Diagnostic, Debug)]
pub enum ConfigError {
    #[error("Failed to parse KDL")]
    #[diagnostic(code(niri_mapper::config::parse_error))]
    ParseError {
        #[source_code]
        src: String,
        #[label("here")]
        span: miette::SourceSpan,
        #[source]
        source: kdl::KdlError,
    },

    #[error("Invalid configuration: {message}")]
    #[diagnostic(code(niri_mapper::config::invalid))]
    Invalid { message: String },

    #[error("Missing required field: {field}")]
    #[diagnostic(code(niri_mapper::config::missing_field))]
    MissingField { field: String },

    #[error("Unknown key: {key}")]
    #[diagnostic(code(niri_mapper::config::unknown_key))]
    UnknownKey { key: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

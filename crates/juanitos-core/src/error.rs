//! Error type shared by all `juanitos-core` modules.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("command `{program}` failed (exit code {code:?}): {stderr}")]
    Command {
        program: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("qube not found: {0}")]
    QubeNotFound(String),

    #[error("qube already exists: {0}")]
    QubeExists(String),

    #[error("invalid PCI address: {0}")]
    InvalidPciAddr(String),

    #[error("invalid qube name: {0}")]
    InvalidName(String),

    #[error("firewall config error: {0}")]
    Firewall(#[from] jos_fwcfg::ParseError),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

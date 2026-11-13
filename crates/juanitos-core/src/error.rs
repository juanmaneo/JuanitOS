// SPDX-License-Identifier: BSD-3-Clause
//! Error type shared by all `juanitos-core` modules.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Toml(toml::de::Error),
    TomlSer(toml::ser::Error),
    Command {
        program: String,
        code: Option<i32>,
        stderr: String,
    },
    QubeNotFound(String),
    QubeExists(String),
    InvalidPciAddr(String),
    InvalidName(String),
    Firewall(jos_fwcfg::ParseError),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Toml(e) => write!(f, "TOML parse error: {e}"),
            Error::TomlSer(e) => write!(f, "TOML serialize error: {e}"),
            Error::Command {
                program,
                code,
                stderr,
            } => write!(
                f,
                "command `{program}` failed (exit code {code:?}): {stderr}"
            ),
            Error::QubeNotFound(name) => write!(f, "qube not found: {name}"),
            Error::QubeExists(name) => write!(f, "qube already exists: {name}"),
            Error::InvalidPciAddr(addr) => write!(f, "invalid PCI address: {addr}"),
            Error::InvalidName(name) => write!(f, "invalid qube name: {name}"),
            Error::Firewall(e) => write!(f, "firewall config error: {e}"),
            Error::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Toml(e) => Some(e),
            Error::TomlSer(e) => Some(e),
            Error::Firewall(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Error::Toml(e)
    }
}

impl From<toml::ser::Error> for Error {
    fn from(e: toml::ser::Error) -> Self {
        Error::TomlSer(e)
    }
}

impl From<jos_fwcfg::ParseError> for Error {
    fn from(e: jos_fwcfg::ParseError) -> Self {
        Error::Firewall(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn every_error_says_what_went_wrong_and_keeps_its_cause() {
        let io = Error::from(std::io::Error::other("disk gone"));
        let de = Error::from(toml::from_str::<toml::Value>("alice =").unwrap_err());
        let ser = Error::from(<toml::ser::Error as serde::ser::Error>::custom("bad"));
        let fw = Error::from(jos_fwcfg::ParseError { what: "bad rule" });
        for (e, msg) in [
            (&io, "I/O error: disk gone"),
            (&de, "TOML parse error: "),
            (&ser, "TOML serialize error: bad"),
            (&fw, "firewall config error: bad rule"),
        ] {
            assert!(e.to_string().starts_with(msg), "{e}");
            assert!(e.source().is_some());
        }
        let plain = [
            (
                Error::Command {
                    program: "zfs".into(),
                    code: Some(1),
                    stderr: "no pool".into(),
                },
                "command `zfs` failed (exit code Some(1)): no pool",
            ),
            (Error::QubeNotFound("alice".into()), "qube not found: alice"),
            (Error::QubeExists("bob".into()), "qube already exists: bob"),
            (
                Error::InvalidPciAddr("chuck".into()),
                "invalid PCI address: chuck",
            ),
            (Error::InvalidName("a b".into()), "invalid qube name: a b"),
            (Error::Other("just this".into()), "just this"),
        ];
        for (e, msg) in plain {
            assert_eq!(e.to_string(), msg);
            assert!(e.source().is_none());
        }
    }
}

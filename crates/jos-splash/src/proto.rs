//! Messages between `jos-splash <cmd>` clients (rc.d scripts) and the
//! daemon, as single UDP-style datagrams on a Unix socket.
//!
//! The format is one line of text so a shell script can also speak it:
//! `stage network`, `status Starting qubes`, `leave qubes`,
//! `finish console`.

#![cfg_attr(not(unix), allow(dead_code))]

use std::fmt;

use jos_gfx::Stage;

/// Default socket path.
pub const SOCKET_PATH: &str = "/var/run/jos-splash.sock";

/// What happens to the screen when the splash ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// Leave the last frame on the framebuffer; the compositor's greeter
    /// takes over the display (switching VTs itself).
    Keep,
    /// Restore the text console (`ttyv0`): no desktop, or the user asked
    /// for it with Esc.
    Console,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Stage(Stage),
    /// Shutdown: turn this stage and the later ones off.
    Leave(Stage),
    Status(String),
    Finish(Finish),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad splash message: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

impl Message {
    pub fn parse(text: &str) -> Result<Message, ParseError> {
        let text = text.trim();
        let (verb, rest) = match text.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (text, ""),
        };
        match verb {
            "stage" => stage(rest).map(Message::Stage),
            "leave" => stage(rest).map(Message::Leave),
            "status" => Ok(Message::Status(rest.to_string())),
            "finish" => match rest {
                "" | "keep" => Ok(Message::Finish(Finish::Keep)),
                "console" => Ok(Message::Finish(Finish::Console)),
                other => Err(ParseError(format!("unknown finish mode {other:?}"))),
            },
            other => Err(ParseError(format!("unknown verb {other:?}"))),
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Message::Stage(s) => format!("stage {}", s.name()),
            Message::Leave(s) => format!("leave {}", s.name()),
            Message::Status(t) => format!("status {t}"),
            Message::Finish(Finish::Keep) => "finish keep".to_string(),
            Message::Finish(Finish::Console) => "finish console".to_string(),
        }
    }
}

fn stage(name: &str) -> Result<Stage, ParseError> {
    Stage::from_name(name).ok_or_else(|| ParseError(format!("unknown stage {name:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = [
            Message::Stage(Stage::Network),
            Message::Leave(Stage::Qubes),
            Message::Status("Starting qubes".into()),
            Message::Finish(Finish::Keep),
            Message::Finish(Finish::Console),
        ];
        for m in msgs {
            assert_eq!(Message::parse(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn lenient_parsing() {
        assert_eq!(
            Message::parse("  stage   storage \n").unwrap(),
            Message::Stage(Stage::Storage)
        );
        assert_eq!(
            Message::parse("finish").unwrap(),
            Message::Finish(Finish::Keep)
        );
        assert_eq!(
            Message::parse("status").unwrap(),
            Message::Status(String::new())
        );
    }

    #[test]
    fn errors() {
        assert!(Message::parse("stage nope").is_err());
        assert!(Message::parse("leave").is_err());
        assert!(Message::parse("finish maybe").is_err());
        assert!(Message::parse("dance").is_err());
        assert!(Message::parse("").is_err());
    }
}

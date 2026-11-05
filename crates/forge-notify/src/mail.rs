//! One e-mail, built so that nothing a person typed can change its
//! structure: the addresses are validated, every dynamic header value is a
//! single line (control characters become spaces), non-ASCII text travels as
//! RFC 2047 encoded words in headers and as base64 in the body, and the
//! `Message-ID` is stable per notification so a relay or a client can
//! recognise a repeated hand-over.

use std::time::SystemTime;

use base64::Engine as _;
use forge_core::config::valid_mailbox;
use forge_errors::{ErrorCode, ForgeError};

/// A subject is cut here (characters).
pub const MAX_SUBJECT_CHARS: usize = 200;

/// A complete message ready for a [`crate::Sender`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    from_address: String,
    from_name: String,
    to_address: String,
    subject: String,
    body: String,
    message_id: String,
    date: SystemTime,
}

fn invalid(field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_mail_field")
        .with_context("field", field)
}

/// Text for a header or a title: one line, no control characters, at most
/// `max` characters (an ellipsis marks a cut).
pub fn one_line(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max * 4));
    let mut pending_space = false;
    let mut count = 0usize;
    for c in text.chars() {
        let blank = c.is_control() || c.is_whitespace() || matches!(c, '\u{2028}' | '\u{2029}');
        if blank {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= max {
            out.push('…');
            return out;
        }
        out.push(c);
        count += 1;
    }
    out
}

impl Mail {
    /// `from_address` / `from_name` are the configuration's; `to_address`
    /// is the recipient's e-mail (checked again here: `FG-SYS-0001`
    /// `invalid_mail_field` when it is not a plain mailbox).
    pub fn new(
        (from_address, from_name): (&str, &str),
        to_address: &str,
        (subject, body): (&str, &str),
        notification: i64,
        date: SystemTime,
    ) -> Result<Self, ForgeError> {
        if !valid_mailbox(from_address) {
            return Err(invalid("from_address"));
        }
        if !valid_mailbox(to_address) {
            return Err(invalid("to_address"));
        }
        let domain = from_address
            .rsplit_once('@')
            .map(|(_, domain)| domain)
            .ok_or_else(|| invalid("from_address"))?;
        Ok(Self {
            from_address: from_address.to_owned(),
            from_name: one_line(from_name, 64),
            to_address: to_address.to_owned(),
            subject: one_line(subject, MAX_SUBJECT_CHARS),
            body: body.replace("\r\n", "\n").replace('\r', "\n"),
            message_id: format!("<notification-{notification}@{domain}>"),
            date,
        })
    }

    pub fn from_address(&self) -> &str {
        &self.from_address
    }

    pub fn to_address(&self) -> &str {
        &self.to_address
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    /// The message as it goes over the wire (CRLF line ends, no dot
    /// stuffing: the transport does that).
    pub fn render(&self) -> Vec<u8> {
        let mut out = String::new();
        let mut header = |name: &str, value: &str| {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        };
        header("Date", &httpdate::fmt_http_date(self.date));
        header(
            "From",
            &format!("{} <{}>", display_name(&self.from_name), self.from_address),
        );
        header("To", &format!("<{}>", self.to_address));
        header("Subject", &encoded_words(&self.subject));
        header("Message-ID", &self.message_id);
        header("MIME-Version", "1.0");
        header("Content-Type", "text/plain; charset=utf-8");
        header("Content-Transfer-Encoding", "base64");
        // RFC 3834: no automatic replies to this message.
        header("Auto-Submitted", "auto-generated");
        header("X-Auto-Response-Suppress", "All");
        out.push_str("\r\n");
        let encoded = base64::engine::general_purpose::STANDARD.encode(self.body.as_bytes());
        for chunk in encoded.as_bytes().chunks(76) {
            // base64 is ASCII, so the chunks are valid UTF-8.
            out.push_str(&String::from_utf8_lossy(chunk));
            out.push_str("\r\n");
        }
        out.into_bytes()
    }
}

/// `"Name"` for plain text, an encoded word otherwise.
fn display_name(name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ' ' || "._-+()'".contains(c));
    if plain {
        format!("\"{name}\"")
    } else if name.is_empty() {
        "\"\"".to_owned()
    } else {
        encoded_words(name)
    }
}

/// An unstructured header value: as is when it is printable ASCII and short,
/// otherwise RFC 2047 B-encoded words (each at most 75 characters, folded
/// with CRLF and a space).
fn encoded_words(text: &str) -> String {
    let ascii = text.chars().all(|c| (' '..='~').contains(&c));
    if ascii && text.len() <= 70 && !text.contains("=?") {
        return text.to_owned();
    }
    const RAW_PER_WORD: usize = 42;
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > RAW_PER_WORD {
            words.push(encode_word(&chunk));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(encode_word(&chunk));
    }
    words.join("\r\n ")
}

fn encode_word(chunk: &str) -> String {
    format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes())
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)] // tests may fail loudly
mod tests {
    use std::time::Duration;

    use super::*;

    fn mail(subject: &str, body: &str) -> Mail {
        Mail::new(
            ("forge@git.example.test", "GitStation"),
            "alice@example.test",
            (subject, body),
            42,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_000_000),
        )
        .expect("mail")
    }

    fn text(mail: &Mail) -> String {
        String::from_utf8(mail.render()).expect("ascii")
    }

    #[test]
    fn one_line_removes_every_way_to_start_a_new_line() {
        assert_eq!(one_line("a\r\nBcc: x@y.test", 100), "a Bcc: x@y.test");
        assert_eq!(one_line("  a\t\tb\u{2028}c\u{0085}d  ", 100), "a b c d");
        assert_eq!(one_line("\u{0}\u{7}x", 100), "x");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("日本語です", 3), "日本語…");
        assert_eq!(one_line("", 3), "");
    }

    #[test]
    fn the_message_has_the_headers_a_relay_expects_and_a_base64_body() {
        let m = mail("Hello", "Line one\nLine two\n");
        let wire = text(&m);
        let (head, body) = wire.split_once("\r\n\r\n").expect("blank line");
        assert!(head.contains("\r\nFrom: \"GitStation\" <forge@git.example.test>\r\n"));
        assert!(head.contains("\r\nTo: <alice@example.test>\r\n"));
        assert!(head.contains("\r\nSubject: Hello\r\n"));
        assert!(head.contains("\r\nMessage-ID: <notification-42@git.example.test>\r\n"));
        assert!(head.starts_with("Date: "), "{head}");
        assert!(head.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(head.contains("Content-Transfer-Encoding: base64"));
        assert!(head.contains("Auto-Submitted: auto-generated"));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body.replace("\r\n", ""))
            .expect("base64");
        assert_eq!(
            String::from_utf8(decoded).expect("utf8"),
            "Line one\nLine two\n"
        );
        for line in wire.split("\r\n") {
            assert!(line.len() <= 998, "line too long");
        }
        for line in body.split("\r\n") {
            assert!(line.len() <= 76, "body line {line}");
        }
        // The id is the notification's: the same on every try.
        assert_eq!(m.message_id(), mail("Other", "x").message_id());
    }

    #[test]
    fn non_ascii_text_travels_as_encoded_words_that_decode_back() {
        let subject =
            "[GitStation] web/app!7 がマージされました — 日本語の長い題名を折り返して送る確認";
        let wire = text(&mail(subject, "本文"));
        let head = wire.split("\r\n\r\n").next().expect("head");
        assert!(head.is_ascii());
        let start = head.find("Subject: ").expect("subject") + "Subject: ".len();
        let unfolded = head[start..]
            .split("\r\nMessage-ID")
            .next()
            .expect("subject value")
            .replace("\r\n ", "");
        let mut decoded = String::new();
        for word in unfolded.split("?=") {
            if let Some(b64) = word.strip_prefix("=?UTF-8?B?") {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .expect("word");
                decoded.push_str(&String::from_utf8(bytes).expect("utf8"));
            }
        }
        assert_eq!(decoded, subject);
        for line in head.split("\r\n") {
            assert!(line.len() <= 78, "folded header line {line:?}");
        }
    }

    #[test]
    fn nothing_typed_by_a_person_can_add_a_header() {
        let m = mail("Fix\r\nBcc: evil@example.test", "body");
        let wire = text(&m);
        assert!(!wire.contains("\r\nBcc:"), "{wire}");
        let named = Mail::new(
            ("forge@git.example.test", "Forge\r\nBcc: evil@example.test"),
            "alice@example.test",
            ("s", "b"),
            1,
            SystemTime::UNIX_EPOCH,
        )
        .expect("mail");
        assert!(!text(&named).contains("\r\nBcc:"));
        // An address that is not a plain mailbox is refused outright.
        for bad in [
            "a@b.test\r\nRCPT TO:<evil@x.test>",
            "a b@c.test",
            "<a@c.test>",
            "a@c.test>\r\nDATA",
            "",
            "ü@c.test",
        ] {
            let err = Mail::new(
                ("forge@git.example.test", "F"),
                bad,
                ("s", "b"),
                1,
                SystemTime::UNIX_EPOCH,
            )
            .expect_err(bad);
            assert_eq!(err.code(), ErrorCode::Internal);
        }
    }

    #[test]
    fn a_body_with_a_leading_dot_line_is_safe_on_the_wire() {
        // Base64 hides the dots; the transport still stuffs them.
        let wire = mail("s", ".\n..\n.x").render();
        let stuffed = crate::smtp::dot_stuffed(&wire);
        assert!(stuffed.ends_with(b"\r\n.\r\n"));
        assert!(!String::from_utf8_lossy(&wire).contains("\r\n.\r\n"));
    }
}

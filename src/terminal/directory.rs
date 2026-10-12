//! Passive working-directory reports. Nothing here writes to the terminal,
//! resolves a hostname, or connects to the authority in an OSC 7 URL.

use percent_encoding::percent_decode_str;
use url::{Host, Url};

/// A directory the terminal's output reported, not proof of the remote host.
/// The owner must validate `authority` against its existing connection before
/// using the path. Empty authorities and `localhost` retain their meaning here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportedDirectory {
    authority: String,
    path: String,
}

impl ReportedDirectory {
    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        // URL parsers can silently remove controls, trim whitespace and treat
        // backslashes as separators. None of those repairs are safe for a path.
        if text
            .chars()
            .any(|ch| ch.is_control() || ch.is_ascii_whitespace())
            || text.contains('\\')
        {
            return None;
        }
        let rest = text.strip_prefix("file://")?;
        let slash = rest.find('/')?;
        let (authority, raw_path) = rest.split_at(slash);
        if authority.contains(['@', '%']) {
            return None;
        }
        // Parse the authority separately: `file://localhost/` is normalized to
        // an empty host by Url, but the consumer needs the original distinction.
        if !authority.is_empty() {
            Host::parse(authority).ok()?;
        }
        let url = Url::parse(text).ok()?;
        if url.scheme() != "file"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !valid_escapes(raw_path.as_bytes())
        {
            return None;
        }
        // Use the original path, not Url::path(): URL normalization collapses
        // dot segments, which can change a POSIX path through symbolic links.
        let path = percent_decode_str(raw_path)
            .decode_utf8()
            .ok()?
            .into_owned();
        if !path.starts_with('/') || path.chars().any(char::is_control) {
            return None;
        }
        Some(Self {
            authority: authority.to_owned(),
            path,
        })
    }
}

fn valid_escapes(bytes: &[u8]) -> bool {
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            if !bytes
                .get(offset + 1..offset + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            {
                return false;
            }
            offset += 3;
        } else {
            offset += 1;
        }
    }
    true
}

const REPORT_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    IgnoredString,
    IgnoredStringEscape,
}

/// Strict, bounded OSC collection across arbitrary transport chunks. VTE's
/// general parser discards embedded control bytes and dispatches an OSC at any
/// ESC; a directory must instead reject the entire malformed report.
#[derive(Default)]
pub(super) struct DirectoryScanner {
    state: State,
    payload: Vec<u8>,
    invalid: bool,
}

impl DirectoryScanner {
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<ReportedDirectory> {
        let mut found = Vec::new();
        for &byte in bytes {
            match self.state {
                State::Ground => {
                    if byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => self.escape(byte),
                State::Osc => match byte {
                    0x07 => {
                        self.finish(&mut found);
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::OscEscape,
                    0x18 | 0x1a => {
                        self.payload.clear();
                        self.state = State::Ground;
                    }
                    0x00..=0x1f | 0x7f => self.invalid = true,
                    _ if self.payload.len() < REPORT_LIMIT && !self.invalid => {
                        self.payload.push(byte);
                    }
                    _ => self.invalid = true,
                },
                State::OscEscape => {
                    if byte == b'\\' {
                        self.finish(&mut found);
                        self.state = State::Ground;
                    } else {
                        // An interrupted OSC is not a partial directory. The
                        // new escape can begin another sequence of its own.
                        self.payload.clear();
                        self.escape(byte);
                    }
                }
                State::IgnoredString => match byte {
                    0x1b => self.state = State::IgnoredStringEscape,
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {}
                },
                State::IgnoredStringEscape => {
                    self.state = match byte {
                        b'\\' | 0x18 | 0x1a => State::Ground,
                        0x1b => State::IgnoredStringEscape,
                        _ => State::IgnoredString,
                    };
                }
            }
        }
        found
    }

    fn escape(&mut self, byte: u8) {
        self.state = match byte {
            b']' => {
                self.payload.clear();
                self.invalid = false;
                State::Osc
            }
            b'P' | b'X' | b'^' | b'_' => State::IgnoredString,
            0x1b => State::Escape,
            _ => State::Ground,
        };
    }

    fn finish(&mut self, found: &mut Vec<ReportedDirectory>) {
        if !self.invalid
            && let Some(url) = self.payload.strip_prefix(b"7;")
            && let Some(directory) = ReportedDirectory::parse(url)
        {
            found.push(directory);
        }
        self.payload.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_paths_without_reinterpreting_remote_posix_names() {
        let directory = ReportedDirectory::parse(
            b"file://server.example/srv/a%20b/%E6%96%87%E4%BB%B6%3B%23%3F%25",
        )
        .unwrap();
        assert_eq!(directory.authority(), "server.example");
        assert_eq!(directory.path(), "/srv/a b/文件;#?%");
        assert_eq!(
            ReportedDirectory::parse(b"file://host/symlink/../other")
                .unwrap()
                .path(),
            "/symlink/../other"
        );
        assert_eq!(
            ReportedDirectory::parse(b"file:///tmp")
                .unwrap()
                .authority(),
            ""
        );
        assert_eq!(
            ReportedDirectory::parse(b"file://localhost/tmp")
                .unwrap()
                .authority(),
            "localhost"
        );
        assert_eq!(
            ReportedDirectory::parse(b"file://[::1]/tmp")
                .unwrap()
                .authority(),
            "[::1]"
        );
    }

    #[test]
    fn rejects_ambiguous_or_malformed_directory_urls() {
        for url in [
            "https://host/tmp",
            "file:/tmp",
            "file://host",
            "file://host:22/tmp",
            "file://user@host/tmp",
            "file://user:secret@host/tmp",
            "file://host/tmp?x",
            "file://host/tmp#x",
            "file://host/a b",
            " file://host/tmp",
            "file://host/a\\b",
            "file://host/a%00b",
            "file://host/a%0Ab",
            "file://host/a%7Fb",
            "file://host/a%C2%85b",
            "file://host/%FF",
            "file://host/%",
            "file://host/%2",
            "file://host/%xx",
            "file://%68ost/tmp",
            "file://host/a\0b",
            "file://host/a\nb",
        ] {
            assert!(
                ReportedDirectory::parse(url.as_bytes()).is_none(),
                "{url:?}"
            );
        }
        assert!(ReportedDirectory::parse(b"file://host/\xff").is_none());
    }

    #[test]
    fn reports_only_after_a_complete_osc_across_every_chunk_boundary() {
        for report in [
            b"before\x1b]7;file://host/a%20b\x07after".as_slice(),
            b"before\x1b]7;file://host/a%20b\x1b\\after".as_slice(),
        ] {
            for split in 0..=report.len() {
                let mut scanner = DirectoryScanner::default();
                let mut found = scanner.scan(&report[..split]);
                found.extend(scanner.scan(&report[split..]));
                assert_eq!(found.len(), 1, "split {split}");
                assert_eq!(found[0].path(), "/a b");
            }
        }
        let mut scanner = DirectoryScanner::default();
        assert!(scanner.scan(b"\x1b]7;file://host/tmp\x1b").is_empty());
        assert_eq!(scanner.scan(b"\\").len(), 1);

        let mut scanner = DirectoryScanner::default();
        let found = "\x1b]7;file://host/文件;a%20b\x07"
            .as_bytes()
            .chunks(1)
            .flat_map(|chunk| scanner.scan(chunk))
            .collect::<Vec<_>>();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path(), "/文件;a b");
    }

    #[test]
    fn rejects_controls_cancellation_overflow_and_unrelated_strings() {
        for bytes in [
            b"\x1b]7;file://host/a\0b\x07".as_slice(),
            b"\x1b]7;file://host/a\nb\x07".as_slice(),
            b"\x1b]7;file://host/a\x18".as_slice(),
            b"\x1b]7;file://host/a\x1bX".as_slice(),
            b"\x1b]0;file://host/tmp\x07".as_slice(),
            b"\x1bP\x1b]7;file://host/tmp\x07\x1b\\".as_slice(),
            b"\x1b]133;A\x07".as_slice(),
        ] {
            assert!(
                DirectoryScanner::default().scan(bytes).is_empty(),
                "{bytes:?}"
            );
        }
        let mut scanner = DirectoryScanner::default();
        let mut too_long = b"\x1b]7;file://host/".to_vec();
        too_long.extend(std::iter::repeat_n(b'x', REPORT_LIMIT));
        assert!(scanner.scan(&too_long).is_empty());
        assert!(scanner.payload.len() <= REPORT_LIMIT);
        assert!(scanner.scan(b"\x07").is_empty());
        assert_eq!(scanner.scan(b"\x1b]7;file://host/ok\x07")[0].path(), "/ok");
    }

    #[test]
    fn a_fresh_connection_does_not_finish_the_previous_connections_osc() {
        let mut previous = DirectoryScanner::default();
        assert!(previous.scan(b"\x1b]7;file://host/old").is_empty());
        let mut current = DirectoryScanner::default();
        assert!(current.scan(b"/tail\x07").is_empty());
        assert_eq!(
            current.scan(b"\x1b]7;file://host/new\x07")[0].path(),
            "/new"
        );
    }
}

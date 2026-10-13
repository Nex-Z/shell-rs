//! Work out which operating system is on the far end.
//!
//! Runs once per successful connection on its own exec channel, so the user's
//! shell never sees it and its output cannot be confused with theirs.

use crate::host::HostOs;

/// One question for POSIX hosts: the kernel name, then whatever
/// `/etc/os-release` says. stderr is dropped because macOS legitimately has no
/// such file, and because a Windows shell will fail both halves.
pub const PROBE_COMMAND: &str = "uname -s; cat /etc/os-release 2>/dev/null";

/// The follow-up when the POSIX answer is unrecognized. Windows OpenSSH wraps
/// exec commands in `cmd.exe /c "..."`: spelling out the executable extension
/// keeps the outer quotes from becoming part of `ver`. `/d` disables AutoRun
/// for this nested interpreter. This also works from PowerShell.
pub const WINDOWS_PROBE_COMMAND: &str = "cmd.exe /d /c ver";

/// Read the POSIX probe. `None` means the output was not recognizable, which
/// is the signal to try [`WINDOWS_PROBE_COMMAND`].
pub fn parse_probe(output: &str) -> Option<HostOs> {
    let kernel = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    match kernel {
        "Darwin" => Some(HostOs::MacOs),
        "FreeBSD" => Some(HostOs::FreeBsd),
        "OpenBSD" => Some(HostOs::OpenBsd),
        "NetBSD" => Some(HostOs::NetBsd),
        // A Linux box with no `/etc/os-release`, or one shipping an id no
        // build of ShellRS knows, still gets the generic mark.
        "Linux" => Some(distribution(output).unwrap_or(HostOs::Linux)),
        _ => None,
    }
}

/// Read the Windows follow-up.
pub fn parse_windows_probe(output: &str) -> Option<HostOs> {
    output
        .lines()
        .any(|line| {
            let Some(version) = line
                .trim()
                .strip_prefix("Microsoft Windows [")
                .and_then(|line| line.strip_suffix(']'))
                .and_then(|line| line.split_whitespace().last())
            else {
                return false;
            };
            // The label before the version is localized; the numeric version is not.
            version.contains('.')
                && version
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .then_some(HostOs::Windows)
}

/// Pick the distribution out of `/etc/os-release`.
///
/// `ID_LIKE` is the fallback for derivatives, so Linux Mint shows Ubuntu and
/// Pop!_OS shows Debian rather than a bare penguin. It only applies when `ID`
/// itself is unknown, so a distribution ShellRS does know is never mistaken for
/// its parent.
fn distribution(output: &str) -> Option<HostOs> {
    let mut like = None;
    for line in output.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("ID=") {
            if let Some(os) = distribution_id(unquote(value)) {
                return Some(os);
            }
        } else if let Some(value) = line.strip_prefix("ID_LIKE=")
            && like.is_none()
        {
            like = unquote(value).split_whitespace().find_map(distribution_id);
        }
    }
    like
}

fn distribution_id(id: &str) -> Option<HostOs> {
    Some(match id {
        "ubuntu" => HostOs::Ubuntu,
        "debian" => HostOs::Debian,
        "fedora" => HostOs::Fedora,
        "rhel" | "redhat" => HostOs::RedHat,
        "centos" => HostOs::CentOs,
        "rocky" => HostOs::Rocky,
        "almalinux" => HostOs::Alma,
        "arch" => HostOs::Arch,
        "alpine" => HostOs::Alpine,
        "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "sles" | "sled" | "suse" => {
            HostOs::Suse
        }
        "gentoo" => HostOs::Gentoo,
        "kali" => HostOs::Kali,
        "manjaro" => HostOs::Manjaro,
        "raspbian" => HostOs::Raspbian,
        _ => return None,
    })
}

/// `/etc/os-release` values may or may not be quoted.
fn unquote(value: &str) -> &str {
    value
        .trim()
        .trim_matches(|character| character == '"' || character == '\'')
}

/// The most a probe will read before answering. `/etc/os-release` is a few
/// hundred bytes; anything past this is a host misbehaving.
const MAX_PROBE_OUTPUT: usize = 8 * 1024;

/// The two-step probe as a state machine, so the transport can drive it from
/// its own IO loop instead of blocking the terminal on a round trip.
pub(super) struct HostOsProbe {
    stage: Stage,
    output: Vec<u8>,
}

enum Stage {
    Posix,
    Windows,
    Done,
}

/// What to do once a probe channel closes.
pub(super) enum ProbeOutcome {
    Detected(HostOs),
    /// The POSIX probe said nothing, which is what a Windows shell looks like.
    AskWindows,
    GaveUp,
}

impl HostOsProbe {
    pub(super) fn new() -> Self {
        Self {
            stage: Stage::Posix,
            output: Vec::new(),
        }
    }

    /// The command to run for the current step, or `None` when finished.
    pub(super) fn command(&self) -> Option<&'static str> {
        match self.stage {
            Stage::Posix => Some(PROBE_COMMAND),
            Stage::Windows => Some(WINDOWS_PROBE_COMMAND),
            Stage::Done => None,
        }
    }

    /// Collect stdout. stderr is deliberately not fed in: on Windows it holds
    /// the shell's complaint about `uname`, which says nothing useful.
    pub(super) fn push(&mut self, bytes: &[u8]) {
        let room = MAX_PROBE_OUTPUT.saturating_sub(self.output.len());
        self.output
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// The channel closed. Answer, or ask for one more step.
    pub(super) fn finish(&mut self) -> ProbeOutcome {
        let output = String::from_utf8_lossy(&self.output);
        let outcome = match self.stage {
            Stage::Posix => match parse_probe(&output) {
                Some(os) => ProbeOutcome::Detected(os),
                None => ProbeOutcome::AskWindows,
            },
            Stage::Windows => match parse_windows_probe(&output) {
                Some(os) => ProbeOutcome::Detected(os),
                None => ProbeOutcome::GaveUp,
            },
            Stage::Done => ProbeOutcome::GaveUp,
        };
        self.output.clear();
        self.stage = match outcome {
            ProbeOutcome::AskWindows => Stage::Windows,
            _ => Stage::Done,
        };
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::{HostOsProbe, MAX_PROBE_OUTPUT, ProbeOutcome, parse_probe, parse_windows_probe};
    use crate::host::HostOs;

    #[test]
    fn reads_ubuntu() {
        let output = "Linux\n\
             PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n\
             NAME=\"Ubuntu\"\n\
             ID=ubuntu\n\
             ID_LIKE=debian\n";
        assert_eq!(parse_probe(output), Some(HostOs::Ubuntu));
    }

    #[test]
    fn reads_a_quoted_id() {
        let output = "Linux\nNAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n";
        assert_eq!(parse_probe(output), Some(HostOs::Rocky));
    }

    #[test]
    fn a_derivative_falls_back_to_what_it_is_like() {
        let output = "Linux\nNAME=\"Linux Mint\"\nID=linuxmint\nID_LIKE=ubuntu\n";
        assert_eq!(parse_probe(output), Some(HostOs::Ubuntu));
    }

    #[test]
    fn an_unknown_distribution_is_still_linux() {
        let output = "Linux\nNAME=\"Something New\"\nID=somethingnew\n";
        assert_eq!(parse_probe(output), Some(HostOs::Linux));
    }

    #[test]
    fn a_linux_without_os_release_is_still_linux() {
        assert_eq!(parse_probe("Linux\n"), Some(HostOs::Linux));
    }

    #[test]
    fn reads_the_bsds_and_macos() {
        assert_eq!(parse_probe("Darwin\n"), Some(HostOs::MacOs));
        assert_eq!(parse_probe("FreeBSD\n"), Some(HostOs::FreeBsd));
        assert_eq!(parse_probe("OpenBSD\n"), Some(HostOs::OpenBsd));
        assert_eq!(parse_probe("NetBSD\n"), Some(HostOs::NetBsd));
    }

    #[test]
    fn leading_blank_lines_are_skipped() {
        assert_eq!(parse_probe("\n\n  Darwin  \n"), Some(HostOs::MacOs));
    }

    #[test]
    fn nothing_useful_means_no_answer() {
        assert_eq!(parse_probe(""), None);
        assert_eq!(parse_probe("\n \n"), None);
        assert_eq!(parse_probe("'uname' is not recognized\n"), None);
    }

    #[test]
    fn the_windows_follow_up_reads_ver() {
        assert_eq!(
            parse_windows_probe("Microsoft Windows [Version 10.0.19045.4291]\n"),
            Some(HostOs::Windows)
        );
        assert_eq!(parse_windows_probe(""), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_probe_works_inside_the_openssh_cmd_wrapper() {
        use std::os::windows::process::CommandExt as _;
        // Windows OpenSSH wraps non-PTY exec requests in cmd.exe /c "...".
        // raw_arg preserves that wrapper rather than applying argv escaping.
        let output = std::process::Command::new("cmd.exe")
            .raw_arg(format!("/d /c \"{}\"", super::WINDOWS_PROBE_COMMAND))
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(
            parse_windows_probe(&String::from_utf8_lossy(&output.stdout)),
            Some(HostOs::Windows)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_probe_works_from_powershell_without_a_profile() {
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                super::WINDOWS_PROBE_COMMAND,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(
            parse_windows_probe(&String::from_utf8_lossy(&output.stdout)),
            Some(HostOs::Windows)
        );
    }

    #[test]
    fn windows_version_labels_can_be_localized() {
        for output in [
            "\r\nMicrosoft Windows [版本 10.0.26100.1]\r\n",
            "Microsoft Windows [Version 6.3.9600]\n",
        ] {
            assert_eq!(parse_windows_probe(output), Some(HostOs::Windows));
        }
    }

    #[test]
    fn windows_mentions_in_errors_are_not_version_answers() {
        for output in [
            "Windows command unavailable",
            "cmd.exe: command not found",
            "error: Microsoft Windows [Version 10.0.26100.1]",
            "Microsoft Windows [Version unavailable]",
        ] {
            assert_eq!(parse_windows_probe(output), None, "{output}");
        }
    }

    #[test]
    fn a_posix_host_answers_in_one_step() {
        let mut probe = HostOsProbe::new();
        assert_eq!(probe.command(), Some(super::PROBE_COMMAND));
        probe.push(b"Linux\nID=debian\n");
        assert!(matches!(
            probe.finish(),
            ProbeOutcome::Detected(HostOs::Debian)
        ));
        assert_eq!(probe.command(), None);
    }

    #[test]
    fn a_silent_posix_probe_moves_on_to_windows() {
        let mut probe = HostOsProbe::new();
        assert!(matches!(probe.finish(), ProbeOutcome::AskWindows));
        assert_eq!(probe.command(), Some(super::WINDOWS_PROBE_COMMAND));

        probe.push(b"Microsoft Windows [Version 10.0.19045.4291]\n");
        assert!(matches!(
            probe.finish(),
            ProbeOutcome::Detected(HostOs::Windows)
        ));
        assert_eq!(probe.command(), None);
    }

    #[test]
    fn a_host_that_answers_nothing_twice_is_left_alone() {
        let mut probe = HostOsProbe::new();
        assert!(matches!(probe.finish(), ProbeOutcome::AskWindows));
        assert!(matches!(probe.finish(), ProbeOutcome::GaveUp));
        assert_eq!(probe.command(), None);
    }

    #[test]
    fn output_is_capped() {
        let mut probe = HostOsProbe::new();
        probe.push(&vec![b'x'; MAX_PROBE_OUTPUT * 2]);
        probe.push(b"Linux\n");
        assert!(matches!(probe.finish(), ProbeOutcome::AskWindows));
    }
}

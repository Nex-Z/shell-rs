//! Opt-in, connection-local Bash directory reports. No command is typed
//! into the user's terminal and no startup file is created or changed.

// This command runs remotely in Bash even when ShellRS runs on Windows.
fn quote_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

/// Start an interactive Bash with an in-memory rc file. Login files are
/// sourced in Bash's usual order before adding our prompt hook. This remains
/// opt-in: forced commands and unusual login-shell checks may not support it.
pub(super) fn command(address: &str) -> String {
    let address = address.trim_end_matches('.');
    let authority = if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]")
    } else {
        url::Host::parse(address)
            .map(|host| host.to_string())
            .unwrap_or_else(|_| address.to_owned())
    };
    let script = format!("{}\n{}", LOGIN_FILES, startup_script(&authority),);
    launch(&script)
}

fn launch(script: &str) -> String {
    let launch = format!(
        "exec bash --rcfile <(printf '%s' {}) -i",
        quote_word(script)
    );
    format!("exec bash --noprofile --norc -c {}", quote_word(&launch))
}

const LOGIN_FILES: &str = r#"if [[ -r /etc/profile ]]; then . /etc/profile; fi
for __shellrs_profile in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -r "$__shellrs_profile" ]]; then . "$__shellrs_profile"; break; fi
done
unset __shellrs_profile"#;

fn startup_script(authority: &str) -> String {
    // Keep the endpoint as literal data. It cannot contribute shell syntax
    // or inject an OSC terminator, even if a saved host has an invalid name.
    let authority: String = authority
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b".-_:[]".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    format!(
        r#"if ! declare -F __shellrs_report_directory >/dev/null; then
__shellrs_report_directory() {{
    local __shellrs_status=$?
    local LC_ALL=C __shellrs_path="$PWD" __shellrs_encoded='' __shellrs_char __shellrs_hex __shellrs_i
    for ((__shellrs_i=0; __shellrs_i<${{#__shellrs_path}}; __shellrs_i++)); do
        __shellrs_char="${{__shellrs_path:__shellrs_i:1}}"
        case "$__shellrs_char" in
            [a-zA-Z0-9/._~-]) __shellrs_encoded+="$__shellrs_char" ;;
            *) printf -v __shellrs_hex '%%%02X' "'$__shellrs_char"; __shellrs_encoded+="$__shellrs_hex" ;;
        esac
    done
    printf '\033]7;file://{authority}%s\007' "$__shellrs_encoded"
    return "$__shellrs_status"
}}
__shellrs_prompt_flags=$(declare -p PROMPT_COMMAND 2>/dev/null)
__shellrs_prompt_flags=${{__shellrs_prompt_flags#declare -}}
__shellrs_prompt_flags=${{__shellrs_prompt_flags%% *}}
case "$__shellrs_prompt_flags" in
    *r*) printf '%s\n' 'ShellRS: PROMPT_COMMAND is read-only; directory reports are unavailable.' >&2 ;;
    *a*) PROMPT_COMMAND+=(__shellrs_report_directory) ;;
    *) PROMPT_COMMAND="${{PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}}__shellrs_report_directory" ;;
esac
unset __shellrs_prompt_flags
fi
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_startup_command_keeps_endpoint_text_out_of_shell_syntax() {
        let script = startup_script("host'; touch /tmp/not-a-command\x07");
        assert!(script.contains("host%27%3B%20touch%20%2Ftmp%2Fnot-a-command%07"));
        assert!(!script.contains("file://host';"));
        assert!(command("2001:db8::1").contains("file://[2001:db8::1]"));
    }

    #[cfg(unix)]
    #[test]
    fn bash_reports_encoded_directories_without_losing_existing_prompt_or_status() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("two words 中文;'x");
        std::fs::create_dir(&directory).unwrap();
        let profile = root.path().join(".bash_profile");
        let original = b"PS1='original prompt> '\n";
        std::fs::write(&profile, original).unwrap();
        let script = format!(
            r#"PS1='original prompt> '
PROMPT_COMMAND='printf "old:%s\n" "$?"; false'
{}
cd {}
false
eval "$PROMPT_COMMAND"
printf 'after:%s\nprompt:%s\n' "$?" "$PS1"
"#,
            startup_script("example.test"),
            quote_word(directory.to_str().unwrap()),
        );
        let output = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &script])
            .env("HOME", root.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("old:1"), "{text:?}");
        assert!(text.contains("\x1b]7;file://example.test/"), "{text:?}");
        assert!(
            text.contains("two%20words%20%E4%B8%AD%E6%96%87%3B%27x\x07"),
            "{text:?}"
        );
        assert!(
            text.contains("after:1\nprompt:original prompt> "),
            "{text:?}"
        );
        assert_eq!(std::fs::read(profile).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn bash_keeps_array_hooks_and_leaves_readonly_hooks_unchanged() {
        use std::process::Command;
        for initial in [
            "PROMPT_COMMAND=('printf first' 'printf second')",
            "readonly PROMPT_COMMAND='printf original'",
        ] {
            let script = format!(
                "{initial}\n{}\ndeclare -p PROMPT_COMMAND",
                startup_script("example.test")
            );
            let output = Command::new("bash")
                .args(["--noprofile", "--norc", "-c", &script])
                .output()
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).unwrap();
            if initial.starts_with("readonly") {
                assert!(text.contains("printf original"));
                assert!(!text.contains("__shellrs_report_directory"));
            } else {
                assert!(text.contains("printf first") && text.contains("printf second"));
                assert!(text.contains("__shellrs_report_directory"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn interactive_bash_startup_uses_only_an_in_memory_rc_and_reports_actual_cd() {
        use std::{
            io::Write as _,
            process::{Command, Stdio},
        };
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("changed directory");
        std::fs::create_dir(&target).unwrap();
        let profile = home.path().join(".bash_profile");
        let original = "PS1='kept prompt> '; PROMPT_COMMAND='printf old-hook'\n";
        std::fs::write(&profile, original).unwrap();
        // Source only the test's own profile, never the developer's login
        // files. The process-substitution launch is the production path.
        let script = format!(
            ". {}\n{}",
            quote_word(profile.to_str().unwrap()),
            startup_script("example.test")
        );
        let mut child = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &launch(&script)])
            .env("HOME", home.path())
            .env("HISTFILE", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "cd {}\nexit",
            quote_word(target.to_str().unwrap())
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stdout.contains("old-hook"), "{stdout:?}");
        assert!(stdout.contains("changed%20directory\x07"), "{stdout:?}");
        assert!(stderr.contains("kept prompt> "), "{stderr:?}");
        assert_eq!(std::fs::read_to_string(profile).unwrap(), original);
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ssh_bash_startup_can_repeat_a_profile_hook_and_is_an_explicit_compatibility_limit() {
        use std::process::Command;
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".bashrc"), "printf RC_READ\n").unwrap();
        std::fs::write(home.path().join(".bash_profile"), ". \"$HOME/.bashrc\"\n").unwrap();
        let output = Command::new("bash")
            .args(["-c", ". \"$HOME/.bash_profile\""])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("SHLVL", "0")
            .env("SSH_CLIENT", "127.0.0.1 1234 22")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "RC_READRC_READ");
    }
}

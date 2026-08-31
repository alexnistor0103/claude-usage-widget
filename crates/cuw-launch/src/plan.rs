//! Building the terminal command. Pure — no spawn, no filesystem writes — so
//! the argv every platform ends up running, and the macOS wrapper script it
//! runs through, are covered by ordinary tests on any host.

use std::ffi::OsString;
use std::path::Path;

use crate::{LaunchError, LaunchRequest, P_CWD, P_NONCE, P_PORT, P_SHIM, P_WRAPPER};

/// A nonce is a redemption code, and it lands on a command line: accept only
/// the charset the daemon mints so nothing else can ride in on it.
const NONCE_MIN: usize = 8;
const NONCE_MAX: usize = 128;

/// Reject a request before a nonce is spent on a command that cannot work.
pub fn validate(req: &LaunchRequest) -> Result<(), LaunchError> {
    let ok_len = (NONCE_MIN..=NONCE_MAX).contains(&req.nonce.len());
    let ok_chars = req
        .nonce
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok_len || !ok_chars {
        return Err(LaunchError::BadNonce);
    }
    if req.port == 0 {
        return Err(LaunchError::BadPort);
    }
    if !req.cwd.is_dir() {
        return Err(LaunchError::BadCwd(req.cwd.clone()));
    }
    Ok(())
}

/// The command for a Windows launch: PowerShell on the generated shim, in a new
/// console. `-NoExit` leaves the window up after `claude` exits — the user asked
/// for a terminal, not a one-shot (SWITCHER §5). `-ExecutionPolicy Bypass`
/// because the shim is a daemon-written file under the data dir, not something
/// the user is expected to sign.
pub fn windows_argv(req: &LaunchRequest, shim: &Path) -> Result<Vec<String>, LaunchError> {
    validate(req)?;
    let shim = path_arg(shim)?;
    let cwd = path_arg(&req.cwd)?;
    let port = req.port.to_string();

    let default = vec![
        "powershell.exe".to_string(),
        "-NoProfile".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-NoExit".to_string(),
        "-File".to_string(),
        shim.clone(),
        "-Nonce".to_string(),
        req.nonce.clone(),
        "-Port".to_string(),
        port.clone(),
        "-Cwd".to_string(),
        cwd.clone(),
    ];

    Ok(match req.terminal.as_deref() {
        Some(over) if !over.is_empty() => {
            let subst = [
                (P_SHIM, shim.as_str()),
                (P_NONCE, req.nonce.as_str()),
                (P_PORT, port.as_str()),
                (P_CWD, cwd.as_str()),
            ];
            let full = over.iter().any(|a| a.contains(P_SHIM));
            apply_override(default, over, &subst, full)
        }
        _ => default,
    })
}

/// The command for a macOS launch: `open -a Terminal` on the per-launch
/// `.command` wrapper. `open` hands the document to LaunchServices, which
/// neither inherits our environment nor forwards arguments — hence both the
/// shim and the wrapper (SWITCHER §5).
///
/// The override rule diverges from Windows on purpose: appending
/// `open -a Terminal <wrapper>` behind a prefix is nonsense, so an override
/// mentioning **any** placeholder is a full command, and any other override is
/// a *launcher* (`open -a iTerm`) that only the wrapper path is appended to.
pub fn macos_argv(
    req: &LaunchRequest,
    wrapper: &Path,
    shim_sh: &Path,
) -> Result<Vec<String>, LaunchError> {
    validate(req)?;
    let wrapper = path_arg(wrapper)?;
    let shim = path_arg(shim_sh)?;
    let cwd = path_arg(&req.cwd)?;
    let port = req.port.to_string();

    let default = vec![
        "open".to_string(),
        "-a".to_string(),
        "Terminal".to_string(),
        wrapper.clone(),
    ];

    Ok(match req.terminal.as_deref() {
        Some(over) if !over.is_empty() => {
            let subst = [
                (P_SHIM, shim.as_str()),
                (P_WRAPPER, wrapper.as_str()),
                (P_NONCE, req.nonce.as_str()),
                (P_PORT, port.as_str()),
                (P_CWD, cwd.as_str()),
            ];
            let full = over
                .iter()
                .any(|a| subst.iter().any(|(p, _)| a.contains(p)));
            apply_override(vec![wrapper.clone()], over, &subst, full)
        }
        _ => default,
    })
}

/// Fold a `settings.session.terminal` override into the platform command.
///
/// `full` says which of the two shapes the override is: a **full** command,
/// used verbatim once the placeholders are substituted, or a **launcher** with
/// `tail` appended. Nothing is ever re-split, so an override can never come
/// apart differently than the user wrote it.
///
/// What marks a full command, and what `tail` is, are per-platform — see
/// [`windows_argv`] and [`macos_argv`].
pub fn apply_override(
    tail: Vec<String>,
    over: &[String],
    subst: &[(&str, &str)],
    full: bool,
) -> Vec<String> {
    let expand = |s: &String| {
        subst
            .iter()
            .fold(s.clone(), |acc, (p, v)| acc.replace(p, v))
    };
    let mut out: Vec<String> = over.iter().map(expand).collect();
    if !full {
        out.extend(tail);
    }
    out
}

/// The per-launch `.command` wrapper macOS opens instead of the shim: `open`
/// forwards no arguments to a document, so the nonce, port and directory have
/// to be baked into a file (SWITCHER §5).
///
/// It removes itself before `exec`, so nothing is left in the data dir once a
/// terminal has picked it up; `$0` rather than a written-out path, because that
/// is the one name the file is certain to have been started under.
pub fn wrapper_body(shim_sh: &str, nonce: &str, port: u16, cwd: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Per-launch wrapper written by cuw-daemon. Holds no secret: the nonce\n\
         # is a single-use redemption code (SWITCHER §4).\n\
         rm -f -- \"$0\"\n\
         exec /bin/sh {} {} {} {}\n",
        sh_quote(shim_sh),
        sh_quote(nonce),
        sh_quote(&port.to_string()),
        sh_quote(cwd),
    )
}

/// Quote one value as a POSIX single-quoted word. A single quote cannot appear
/// inside such a word, so it is closed, escaped and reopened — the `'"'"'`
/// idiom, which needs no backslash and so survives every shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Join argv into one command line the way `CommandLineToArgvW` reads it back:
/// quote an argument containing whitespace or a quote, double the backslashes
/// that precede a quote, and escape the quote itself.
///
/// Needed because the spawn goes through `CreateProcessW` rather than
/// `std::process::Command` — see `windows::spawn` for why.
pub fn quote_argv(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        quote_arg(arg, &mut out);
    }
    out
}

fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut slashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                slashes += 1;
                out.push('\\');
            }
            '"' => {
                // Double the run that got us here, then escape the quote.
                for _ in 0..=slashes {
                    out.push('\\');
                }
                out.push('"');
                slashes = 0;
            }
            _ => {
                slashes = 0;
                out.push(c);
            }
        }
    }
    // A trailing run would otherwise escape the closing quote.
    for _ in 0..slashes {
        out.push('\\');
    }
    out.push('"');
}

/// The session's environment: the daemon's, minus anything that would bind
/// another identity (the shim scrubs these too, but a custom terminal sits in
/// between). Sorted case-insensitively, as a Windows environment block must be.
pub fn child_env<I>(vars: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut kept: Vec<(OsString, OsString)> = vars
        .into_iter()
        .filter(|(k, _)| {
            let k = k.to_string_lossy();
            !crate::SCRUBBED_ENV
                .iter()
                .any(|s| k.eq_ignore_ascii_case(s))
        })
        .collect();
    kept.sort_by_key(|(k, _)| k.to_string_lossy().to_lowercase());
    kept
}

/// argv is `String` so it can be asserted on; a path that is not UTF-8 cannot
/// be reasoned about here and is a request error, not a spawn failure.
fn path_arg(p: &Path) -> Result<String, LaunchError> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| LaunchError::BadCwd(p.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn req(nonce: &str) -> LaunchRequest {
        LaunchRequest::new(nonce, 8787, std::env::temp_dir())
    }

    fn shim() -> PathBuf {
        PathBuf::from(r"C:\data\cuw\shim\session-shim.ps1")
    }

    /// A space in the path on purpose: `Application Support` is where the data
    /// dir actually lives on macOS.
    fn shim_sh() -> PathBuf {
        PathBuf::from("/Users/x/Library/Application Support/cuw/shim/session-shim.sh")
    }

    fn wrapper() -> PathBuf {
        PathBuf::from("/Users/x/Library/Application Support/cuw/shim/claude-4242-1.command")
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn default_command_runs_the_shim_in_a_new_powershell() {
        let argv = windows_argv(&req("nonce-abcd1234"), &shim()).expect("argv");
        assert_eq!(argv[0], "powershell.exe");
        assert!(argv.contains(&"-NoProfile".to_string()));
        assert!(argv.contains(&"-NoExit".to_string()));
        let file = argv.iter().position(|a| a == "-File").expect("-File");
        assert_eq!(argv[file + 1], shim().to_string_lossy());
        let nonce = argv.iter().position(|a| a == "-Nonce").expect("-Nonce");
        assert_eq!(argv[nonce + 1], "nonce-abcd1234");
        let port = argv.iter().position(|a| a == "-Port").expect("-Port");
        assert_eq!(argv[port + 1], "8787");
    }

    #[test]
    fn no_token_shaped_argument_is_ever_built() {
        let argv = windows_argv(&req("nonce-abcd1234"), &shim()).expect("argv");
        for a in &argv {
            assert!(!a.contains("sk-ant"), "{a}");
            assert!(!a.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{a}");
        }
    }

    #[test]
    fn a_bad_nonce_is_rejected_before_anything_is_spawned() {
        for bad in [
            "short",
            "",
            &"a".repeat(129),
            "has space",
            "semi;colon",
            "q\"uote",
        ] {
            let e = windows_argv(&req(bad), &shim()).expect_err(bad);
            assert!(matches!(e, LaunchError::BadNonce), "{bad}: {e}");
        }
    }

    #[test]
    fn a_missing_working_directory_is_an_error_not_a_spawn_failure() {
        let mut r = req("nonce-abcd1234");
        r.cwd = std::env::temp_dir().join("cuw-does-not-exist-9f2a");
        assert!(matches!(
            windows_argv(&r, &shim()),
            Err(LaunchError::BadCwd(_))
        ));
    }

    #[test]
    fn port_zero_is_rejected() {
        let mut r = req("nonce-abcd1234");
        r.port = 0;
        assert!(matches!(
            windows_argv(&r, &shim()),
            Err(LaunchError::BadPort)
        ));
    }

    #[test]
    fn a_prefix_override_wraps_the_default_command() {
        let r = req("nonce-abcd1234").with_terminal(Some(vec![
            "wt.exe".into(),
            "-w".into(),
            "0".into(),
            "nt".into(),
        ]));
        let argv = windows_argv(&r, &shim()).expect("argv");
        assert_eq!(&argv[..4], &["wt.exe", "-w", "0", "nt"]);
        assert_eq!(argv[4], "powershell.exe");
        assert!(argv.contains(&"nonce-abcd1234".to_string()));
    }

    #[test]
    fn a_shim_placeholder_override_replaces_the_default_command() {
        let r = req("nonce-abcd1234").with_terminal(Some(vec![
            "alacritty".into(),
            "-e".into(),
            "pwsh".into(),
            "-File".into(),
            P_SHIM.into(),
            "-Nonce".into(),
            P_NONCE.into(),
            "-Port".into(),
            P_PORT.into(),
            "-Cwd".into(),
            P_CWD.into(),
        ]));
        let argv = windows_argv(&r, &shim()).expect("argv");
        assert_eq!(argv[0], "alacritty");
        assert!(!argv.contains(&"powershell.exe".to_string()));
        assert!(!argv.iter().any(|a| a.contains('{')), "{argv:?}");
        assert!(argv.contains(&shim().to_string_lossy().to_string()));
        assert!(argv.contains(&"nonce-abcd1234".to_string()));
        assert!(argv.contains(&"8787".to_string()));
    }

    #[test]
    fn an_override_argument_is_never_re_split() {
        let r = req("nonce-abcd1234")
            .with_terminal(Some(vec![r"C:\Program Files\My Term\term.exe".into()]));
        let argv = windows_argv(&r, &shim()).expect("argv");
        assert_eq!(argv[0], r"C:\Program Files\My Term\term.exe");
        assert_eq!(argv[1], "powershell.exe");
    }

    #[test]
    fn quoting_survives_a_round_trip_through_commandlinetoargvw() {
        let argv: Vec<String> = vec![
            r"C:\Program Files\My Term\term.exe".into(),
            "plain".into(),
            r"ends with backslash\".into(),
            r#"has "quotes" inside"#.into(),
            r#"tricky\"\\"#.into(),
            "".into(),
        ];
        let line = quote_argv(&argv);
        assert_eq!(split_like_windows(&line), argv);
    }

    #[test]
    fn quoting_leaves_ordinary_arguments_untouched() {
        let argv: Vec<String> = vec!["powershell.exe".into(), "-NoProfile".into(), "8787".into()];
        assert_eq!(quote_argv(&argv), "powershell.exe -NoProfile 8787");
    }

    /// `CommandLineToArgvW`'s documented rules, so the quoting above is checked
    /// against the parser it has to satisfy rather than against itself.
    fn split_like_windows(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut started = false;
        let mut slashes = 0usize;
        let flush = |cur: &mut String, args: &mut Vec<String>, started: &mut bool| {
            if *started {
                args.push(std::mem::take(cur));
                *started = false;
            }
        };
        for c in line.chars() {
            match c {
                '\\' => {
                    slashes += 1;
                    started = true;
                }
                '"' => {
                    cur.push_str(&"\\".repeat(slashes / 2));
                    if slashes % 2 == 1 {
                        cur.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    slashes = 0;
                    started = true;
                }
                ' ' | '\t' if !in_quotes => {
                    cur.push_str(&"\\".repeat(slashes));
                    slashes = 0;
                    flush(&mut cur, &mut args, &mut started);
                }
                _ => {
                    cur.push_str(&"\\".repeat(slashes));
                    slashes = 0;
                    cur.push(c);
                    started = true;
                }
            }
        }
        cur.push_str(&"\\".repeat(slashes));
        flush(&mut cur, &mut args, &mut started);
        args
    }

    #[test]
    fn the_macos_default_command_opens_terminal_on_the_wrapper() {
        let argv = macos_argv(&req("nonce-abcd1234"), &wrapper(), &shim_sh()).expect("argv");
        assert_eq!(&argv[..3], &["open", "-a", "Terminal"]);
        assert_eq!(argv[3], wrapper().to_string_lossy());
        assert_eq!(argv.len(), 4);
        // The shim is reached through the wrapper, never named on the argv.
        assert!(!argv.iter().any(|a| a.contains("session-shim.sh")));
    }

    #[test]
    fn a_macos_launcher_override_gets_only_the_wrapper_appended() {
        let r = req("nonce-abcd1234").with_terminal(Some(s(&["open", "-a", "iTerm"])));
        let argv = macos_argv(&r, &wrapper(), &shim_sh()).expect("argv");
        // Not `open -a iTerm open -a Terminal <wrapper>`: the whole point of the
        // divergence from Windows.
        assert_eq!(&argv[..3], &["open", "-a", "iTerm"]);
        assert_eq!(argv[3], wrapper().to_string_lossy());
        assert_eq!(argv.len(), 4);
    }

    #[test]
    fn any_placeholder_makes_a_macos_override_a_full_command() {
        for over in [
            s(&["kitty", "--", "/bin/sh", P_SHIM, P_NONCE, P_PORT, P_CWD]),
            s(&["open", "-a", "iTerm", "-n", "--args", P_WRAPPER]),
        ] {
            let r = req("nonce-abcd1234").with_terminal(Some(over.clone()));
            let argv = macos_argv(&r, &wrapper(), &shim_sh()).expect("argv");
            assert_eq!(argv.len(), over.len(), "{argv:?}");
            assert!(!argv.iter().any(|a| a.contains('{')), "{argv:?}");
            assert!(!argv.contains(&"Terminal".to_string()), "{argv:?}");
        }
    }

    #[test]
    fn a_macos_override_argument_is_never_re_split() {
        let r =
            req("nonce-abcd1234").with_terminal(Some(s(&["/Applications/My Term.app/term", "-e"])));
        let argv = macos_argv(&r, &wrapper(), &shim_sh()).expect("argv");
        assert_eq!(argv[0], "/Applications/My Term.app/term");
        assert_eq!(argv[2], wrapper().to_string_lossy());
    }

    #[test]
    fn a_bad_macos_request_is_rejected_before_a_wrapper_is_named() {
        assert!(matches!(
            macos_argv(&req("short"), &wrapper(), &shim_sh()),
            Err(LaunchError::BadNonce)
        ));
        let mut r = req("nonce-abcd1234");
        r.port = 0;
        assert!(matches!(
            macos_argv(&r, &wrapper(), &shim_sh()),
            Err(LaunchError::BadPort)
        ));
    }

    #[test]
    fn the_wrapper_execs_the_shim_and_deletes_itself_first() {
        let body = wrapper_body(
            "/data/cuw/shim/session-shim.sh",
            "nonce-abcd1234",
            8787,
            "/Users/x/code",
        );
        assert!(body.starts_with("#!/bin/sh\n"));
        let del = body.find("rm -f -- \"$0\"").expect("self-delete");
        let exec = body.find("exec /bin/sh").expect("exec");
        assert!(del < exec, "the wrapper outlives its own launch");
        assert!(body.ends_with(
            "exec /bin/sh '/data/cuw/shim/session-shim.sh' 'nonce-abcd1234' '8787' '/Users/x/code'\n"
        ));
        assert!(!body.contains("sk-ant"));
    }

    #[test]
    fn the_wrapper_quotes_a_directory_that_would_otherwise_split() {
        let body = wrapper_body(
            "/s/session-shim.sh",
            "nonce-abcd1234",
            8787,
            "/tmp/it's mine",
        );
        assert!(body.contains(r#"'/tmp/it'"'"'s mine'"#), "{body}");
    }

    #[test]
    fn single_quoting_closes_escapes_and_reopens() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote(r"back\slash"), r"'back\slash'");
        assert_eq!(sh_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(sh_quote("'"), r#"''"'"''"#);
        // Nothing a shell would still read as syntax survives outside a quote.
        for hostile in ["$(id)", "`id`", "a; rm -rf /", "$HOME", "a\nb"] {
            let q = sh_quote(hostile);
            assert!(q.starts_with('\'') && q.ends_with('\''), "{q}");
        }
    }

    /// Checks the quoting against the parser it has to satisfy rather than
    /// against itself. Silently skipped where `sh` is unavailable.
    #[cfg(unix)]
    #[test]
    fn single_quoting_survives_a_round_trip_through_sh() {
        for value in ["plain", "a b", "it's", "'", r"back\slash", "$(id)", "`id`"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(value)))
                .output();
            let Ok(out) = out else {
                return;
            };
            assert_eq!(String::from_utf8_lossy(&out.stdout), value);
        }
    }

    #[test]
    fn the_child_environment_drops_every_conflicting_identity() {
        let vars: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), "C:\\bin".into()),
            ("anthropic_api_key".into(), "leftover".into()),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "leftover".into()),
            ("CLAUDE_CODE_USE_VERTEX".into(), "1".into()),
            ("HOME".into(), "C:\\Users\\x".into()),
        ];
        let kept = child_env(vars);
        let names: Vec<String> = kept
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["HOME", "PATH"]);
    }

    #[test]
    fn the_child_environment_is_sorted_case_insensitively() {
        let vars: Vec<(OsString, OsString)> = vec![
            ("Zeta".into(), "1".into()),
            ("alpha".into(), "1".into()),
            ("Beta".into(), "1".into()),
        ];
        let names: Vec<String> = child_env(vars)
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["alpha", "Beta", "Zeta"]);
    }
}

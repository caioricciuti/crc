//! A terminal inside `ssh`: which host, as which user, since when. Read
//! from this Mac alone, the ssh process's own command line and
//! ~/.ssh/config; nothing is asked of the server.

use std::path::Path;
use std::time::Instant;

/// An ssh session a terminal is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// What was typed: `root@62.238.118.239`, or an alias from the config.
    pub destination: String,
    pub user: Option<String>,
    /// The host connected to: the config's HostName for an alias.
    pub host: String,
    pub port: Option<u16>,
    pub since: Instant,
}

impl Remote {
    /// `user@host`, or the host alone.
    pub fn login(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// How long it has been connected, as a row says it: `5 min`, `2 h`.
    pub fn connected_for(&self) -> String {
        let s = self.since.elapsed().as_secs();
        match s {
            0..60 => "just now".into(),
            60..3600 => format!("{} min", s / 60),
            _ => format!("{} h {} min", s / 3600, s / 60 % 60),
        }
    }
}

/// What an ssh command line names, before the config is read.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Target {
    pub destination: String,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// ssh's options that take a value, from ssh(1).
const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The destination of `ssh` run with `args` (without the program): the
/// first word that is not an option, `-l` and `-p` taken into account.
pub fn parse(args: &[String]) -> Option<Target> {
    let mut user = None;
    let mut port = None;
    let mut words = args.iter();
    let mut destination = None;
    while let Some(word) = words.next() {
        if word == "--" {
            destination = words.next().cloned();
            break;
        }
        let Some(flags) = word.strip_prefix('-').filter(|f| !f.is_empty()) else {
            destination = Some(word.clone());
            break;
        };
        // Flags run together (`-tt`); one that takes a value takes the
        // rest of the word, or the next word.
        for (at, flag) in flags.char_indices() {
            if WITH_VALUE.contains(flag) {
                let rest = &flags[at + flag.len_utf8()..];
                let value = if rest.is_empty() {
                    words.next().cloned()
                } else {
                    Some(rest.to_owned())
                };
                match flag {
                    'l' => user = value,
                    'p' => port = value.and_then(|v| v.parse().ok()),
                    _ => {}
                }
                break;
            }
        }
    }
    let destination = destination?;
    let rest = destination.strip_prefix("ssh://").unwrap_or(&destination);
    let (target_user, mut host) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_owned()), h.to_owned()),
        None => (None, rest.to_owned()),
    };
    if destination.starts_with("ssh://")
        && let Some((h, p)) = host.rsplit_once(':')
        && let Ok(p) = p.parse()
    {
        port = port.or(Some(p));
        host = h.to_owned();
    }
    Some(Target {
        destination,
        user: target_user.or(user),
        host,
        port,
    })
}

/// What ~/.ssh/config says for `alias`: its HostName, User and Port, from
/// the first `Host` line that names it exactly. Patterns are not matched.
pub fn from_config(text: &str, alias: &str) -> (Option<String>, Option<String>, Option<u16>) {
    let (mut host, mut user, mut port) = (None, None, None);
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        let value = value.trim().trim_start_matches('=').trim();
        match key.to_ascii_lowercase().as_str() {
            "host" | "match" => {
                if inside {
                    break;
                }
                inside = key.eq_ignore_ascii_case("host")
                    && value.split_whitespace().any(|name| name == alias);
            }
            "hostname" if inside => host = host.or(Some(value.to_owned())),
            "user" if inside => user = user.or(Some(value.to_owned())),
            "port" if inside => port = port.or(value.parse().ok()),
            _ => {}
        }
    }
    (host, user, port)
}

/// The hosts ~/.ssh/config names: every name on its `Host` lines, in
/// order, the patterns (`*`, `?`, `!`) left out.
pub fn config_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("host") {
            continue;
        }
        for name in value.trim().trim_start_matches('=').split_whitespace() {
            if !name.contains(['*', '?', '!']) && !out.iter().any(|n| n == name) {
                out.push(name.to_owned());
            }
        }
    }
    out
}

/// The hosts this Mac's ~/.ssh/config names (see [`config_hosts`]).
pub fn known_hosts() -> Vec<String> {
    std::env::var_os("HOME")
        .map(|h| Path::new(&h).join(".ssh/config"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map_or_else(Vec::new, |text| config_hosts(&text))
}

/// What to say of `host`, typed for a remote workspace, when ssh will
/// likely not find it: not `user@host`, not a name with a dot or an
/// address, and not one of ~/.ssh/config's `hosts`. The config's hosts are
/// offered instead; `None` when it looks fine.
pub fn unknown_host(host: &str, hosts: &[String]) -> Option<String> {
    if host.contains(['@', '.', ':']) || hosts.iter().any(|h| h == host) {
        return None;
    }
    Some(match hosts {
        [] => format!("{host} is not in ~/.ssh/config, which names no hosts"),
        [one] => format!("{host} is not in ~/.ssh/config: did you mean {one}?"),
        many if many.len() <= 5 => format!(
            "{host} is not in ~/.ssh/config: did you mean one of {}?",
            many.join(", ")
        ),
        many => format!(
            "{host} is not in ~/.ssh/config: did you mean one of {}, \u{2026}?",
            many[..5].join(", ")
        ),
    })
}

/// The ssh session the shell `pid` is running, if it is running one now.
pub fn of_shell(pid: i32) -> Option<Target> {
    crate::platform::child_pids(pid)
        .into_iter()
        .find_map(|child| {
            let args = crate::platform::process_args(child)?;
            let program = Path::new(args.first()?).file_name()?.to_str()?.to_owned();
            if program != "ssh" {
                return None;
            }
            let mut target = parse(&args[1..])?;
            let config = std::env::var_os("HOME")
                .map(|h| Path::new(&h).join(".ssh/config"))
                .and_then(|p| std::fs::read_to_string(p).ok());
            if let Some(config) = config {
                let (host, user, port) = from_config(&config, &target.host);
                if let Some(host) = host {
                    target.host = host;
                }
                target.user = target.user.or(user);
                target.port = target.port.or(port);
            }
            Some(target)
        })
}

/// The host and folder of a remote workspace, from what the New Workspace
/// prompt was given: `host:/abs/path`, `host:~/path` or `host:~`. The host
/// is an alias from ~/.ssh/config or `user@host`, no port; one that could
/// pass for an option, or that has spaces or quotes, is refused.
pub fn workspace_target(text: &str) -> Option<(String, String)> {
    let (host, path) = text.trim().split_once(':')?;
    let bad = |c: char| c.is_whitespace() || c.is_control() || "'\"`".contains(c);
    if host.is_empty() || host.starts_with('-') || host.contains(bad) {
        return None;
    }
    if !(path.starts_with('/') || path.starts_with("~/") || path == "~")
        || path.contains(|c: char| c.is_control())
    {
        return None;
    }
    Some((host.to_owned(), path.to_owned()))
}

/// What a remote workspace is called: its folder's name, else the host's.
pub fn workspace_name(host: &str, path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && *n != "~")
        .unwrap_or(host)
        .to_owned()
}

/// `text` in single quotes for a POSIX shell. A quote in it closes them and
/// is put in double ones (`'"'"'`), not after a backslash: what ssh hands
/// the host's login shell holds these lines quoted once more, and fish
/// reads a backslash before a quote inside single quotes as an escape.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r#"'"'"'"#))
}

/// What ssh is given for `line`, a POSIX shell line: `sh -c` and the line,
/// quoted. ssh hands it to the host user's login shell, which may be csh or
/// fish, so it has to read the same in all of them. Single quotes do, but
/// for a few characters: csh expands `!` and stops at a line break even
/// inside them, and fish takes a backslash before a backslash or a quote as
/// an escape. A line with any of those (or any other control character)
/// goes as octal escapes instead, which `sh` turns back into the line and
/// runs; the backslash `printf` needs for them is made by `awk`, as none
/// can be written. Trailing line breaks of such a line are dropped.
pub fn for_any_shell(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let plain = chars.iter().enumerate().all(|(at, c)| match c {
        '!' => false,
        '\\' => !matches!(chars.get(at + 1), None | Some('\\' | '\'')),
        c => !c.is_control(),
    });
    if plain {
        return format!("sh -c {}", quote(line));
    }
    let mut format = String::new();
    for byte in line.bytes() {
        // A dash could start the format, and printf take it for an option.
        if byte.is_ascii_alphanumeric() || b" _./:=,+@".contains(&byte) {
            format.push(byte as char);
        } else {
            format.push_str(&format!("${{b}}{byte:03o}"));
        }
    }
    let script =
        format!("b=$(LC_ALL=C awk 'BEGIN{{printf \"%c\",92}}'); eval \"$(printf \"{format}\")\"");
    format!("sh -c {}", quote(&script))
}

/// A remote folder for the remote shell's `cd`: quoted, with a leading `~/`
/// left outside the quotes so the shell there expands it.
pub fn shell_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some("") => "~/".into(),
        Some(rest) => format!("~/{}", quote(rest)),
        None if path == "~" => "~".into(),
        None => quote(path),
    }
}

/// What ssh runs on the host: the login shell in `path`, running `command`
/// (`claude`) when given. The `cd` is `sh`'s (see [`for_any_shell`]), so a
/// login shell of another syntax still gets there.
pub fn remote_command(path: &str, command: Option<&str>) -> String {
    let mut line = format!("cd {} && exec \"${{SHELL:-/bin/sh}}\" -l", shell_path(path));
    if let Some(command) = command {
        line.push_str(&format!(" -i -c {}", quote(command)));
    }
    for_any_shell(&line)
}

/// A Unix socket's path, with the suffix ssh adds while it makes one, must
/// fit in `sun_path` (104 bytes on macOS).
const SOCKET_ROOM: usize = 104 - 1 - 17;

/// The socket the connections to `host` share, in `dir`: named after a hash
/// of the host, not ssh's `%C` (40 characters), so a path under Application
/// Support still fits a socket's. `None` when even that does not fit.
pub fn control_path(dir: &Path, host: &str) -> Option<String> {
    // FNV-1a: stable across runs, unlike the standard library's hasher.
    let hash = host.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let path = format!("{}/cm-{hash:016x}", dir.to_str()?);
    (path.len() <= SOCKET_ROOM && !path.contains('"')).then_some(path)
}

/// The socket the connections to `host` share, in a folder of Application
/// Support only you can open, made when it is not there yet.
pub fn control_for(host: &str) -> Option<String> {
    let dir = crate::platform::app_support()?.join("ssh");
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    control_path(&dir, host)
}

/// The ssh to run: CRC_SSH_COMMAND when set (a self-test's stand-in), else
/// the system's.
pub fn program() -> std::path::PathBuf {
    std::env::var_os("CRC_SSH_COMMAND")
        .map_or_else(|| "/usr/bin/ssh".into(), std::path::PathBuf::from)
}

/// The options that share one connection per host through `control`.
fn control_options(args: &mut Vec<String>, control: Option<&str>) {
    if let Some(control) = control {
        for option in [
            "ControlMaster=auto".to_owned(),
            // Quoted: the folder may have a space, as Application Support does.
            format!("ControlPath=\"{control}\""),
            "ControlPersist=600".to_owned(),
        ] {
            args.push("-o".into());
            args.push(option);
        }
    }
}

/// The arguments for ssh that open a terminal in `path` on `host` (or run
/// `command` there through the login shell), sharing one connection per
/// host through `control` (see [`control_path`]) when given.
pub fn ssh_args(
    host: &str,
    path: &str,
    command: Option<&str>,
    control: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["-t".to_owned()];
    control_options(&mut args, control);
    args.push("--".into());
    args.push(host.to_owned());
    args.push(remote_command(path, command));
    args
}

/// What `/bin/sh` runs for a remote terminal: ssh, given as `ssh` and
/// `args`, and when ssh itself fails (status 255) the tab stays, saying
/// so under ssh's own error, until Return: a host it could not reach
/// within seconds gets what to check, with ~/.ssh/config's `hosts`; a
/// connection lost later, that it closed. Without this the tab would go
/// with ssh, before its error could be read.
pub fn kept_open_args(ssh: &Path, args: Vec<String>, host: &str, hosts: &[String]) -> Vec<String> {
    let hint = match hosts {
        [] => "Check the host name, and that ssh reaches it from a terminal.".to_owned(),
        hosts => format!(
            "Check the host name; your ~/.ssh/config hosts are: {}",
            hosts.join(", ")
        ),
    };
    let mut out = vec![
        "-c".to_owned(),
        KEPT_OPEN.to_owned(),
        // `$0`, then what the script is told.
        "crc-ssh".to_owned(),
        format!("crc: could not connect to {host}. {hint}"),
        format!("crc: the connection to {host} closed."),
        ssh.to_string_lossy().into_owned(),
    ];
    out.extend(args);
    out
}

/// The script of [`kept_open_args`]: `$1` what to say when ssh could not
/// connect, `$2` when it lost the connection, then ssh's command line.
const KEPT_OPEN: &str = "failed=$1; lost=$2; shift 2; started=$(date +%s); \"$@\"; s=$?; \
     if [ \"$s\" -eq 255 ]; then \
     if [ $(( $(date +%s) - started )) -lt 20 ]; then printf '\\n%s\\n' \"$failed\"; \
     else printf '\\n%s\\n' \"$lost\"; fi; \
     printf 'Press Return to close this tab.'; read _; fi; exit \"$s\"";

/// Whether a terminal's command line is ssh's: ssh itself, or ssh under
/// [`kept_open_args`].
pub fn is_ssh_launch(program: &Path, args: &[String]) -> bool {
    program.file_name().is_some_and(|n| n == "ssh") || args.get(2).is_some_and(|a| a == "crc-ssh")
}

/// The arguments for ssh that run `line` on `host` with no terminal and
/// nothing asked: a host that wants a password or a new key's approval
/// fails at once instead, as one that does not answer does in seconds.
/// The connection is the one the host's terminals share, when given.
pub fn batch_args(host: &str, control: Option<&str>, line: &str) -> Vec<String> {
    let mut args = Vec::new();
    for option in [
        "BatchMode=yes",
        "ConnectTimeout=5",
        "ServerAliveInterval=5",
        "ServerAliveCountMax=2",
    ] {
        args.push("-o".to_owned());
        args.push(option.to_owned());
    }
    control_options(&mut args, control);
    args.push("--".into());
    args.push(host.to_owned());
    args.push(for_any_shell(line));
    args
}

/// The arguments for ssh that run `line` on `host` to change something
/// there: asking nothing, as [`batch_args`], but never starting the shared
/// connection. A connection ssh keeps open for later holds the stderr of
/// the call that started it, and a write's stderr is read for Git's
/// errors. With `ControlMaster=no` the call goes through `control` when
/// the host's terminals or reads have it open, and on a connection of its
/// own, closed with it, when not; options given here outrank the host's
/// own in ~/.ssh/config.
pub fn write_args(host: &str, control: Option<&str>, line: &str) -> Vec<String> {
    let mut args = Vec::new();
    for option in [
        "BatchMode=yes",
        "ConnectTimeout=5",
        "ServerAliveInterval=5",
        "ServerAliveCountMax=2",
        "ControlMaster=no",
    ] {
        args.push("-o".to_owned());
        args.push(option.to_owned());
    }
    if let Some(control) = control {
        args.push("-o".to_owned());
        args.push(format!("ControlPath=\"{control}\""));
    }
    args.push("--".into());
    args.push(host.to_owned());
    args.push(for_any_shell(line));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_workspace_is_typed_as_host_and_folder() {
        let t = |s: &str| workspace_target(s);
        assert_eq!(
            t("myhost:/srv/app"),
            Some(("myhost".into(), "/srv/app".into()))
        );
        assert_eq!(
            t(" me@box:~/code "),
            Some(("me@box".into(), "~/code".into()))
        );
        assert_eq!(t("box:~"), Some(("box".into(), "~".into())));
        for bad in [
            "-oProxyCommand=x:/a",
            "my host:/a",
            "h'x:/a",
            "h\"x:/a",
            ":/a",
            "box:relative",
            "box:",
            "fix: the login",
            "plain name",
            "box:/a\nb",
        ] {
            assert_eq!(t(bad), None, "{bad:?}");
        }
        assert_eq!(workspace_name("box", "/srv/app/"), "app");
        assert_eq!(workspace_name("box", "~/code"), "code");
        assert_eq!(workspace_name("box", "~"), "box");
        assert_eq!(workspace_name("box", "/"), "box");
    }

    #[test]
    fn a_remote_folder_is_quoted_for_the_shell_there() {
        assert_eq!(shell_path("/srv/app"), "'/srv/app'");
        assert_eq!(shell_path("/srv/my app"), "'/srv/my app'");
        assert_eq!(shell_path("/srv/it's"), r#"'/srv/it'"'"'s'"#);
        assert_eq!(shell_path("/srv/$HOME`x`"), "'/srv/$HOME`x`'");
        assert_eq!(shell_path("~/x"), "~/'x'");
        assert_eq!(shell_path("~/a b/$c"), "~/'a b/$c'");
        assert_eq!(shell_path("~"), "~");
        assert_eq!(
            remote_command("/srv/app", None),
            r#"sh -c 'cd '"'"'/srv/app'"'"' && exec "${SHELL:-/bin/sh}" -l'"#
        );
        assert_eq!(
            remote_command("~/x", Some("claude")),
            r#"sh -c 'cd ~/'"'"'x'"'"' && exec "${SHELL:-/bin/sh}" -l -i -c '"'"'claude'"'"''"#
        );
    }

    /// What `shell -c` prints for `text`, `None` when there is no such
    /// shell here.
    fn run_in(shell: &str, text: &str) -> Option<(bool, String)> {
        let out = std::process::Command::new(shell)
            .arg("-c")
            .arg(text)
            .env("SHELL", "/bin/sh")
            .output()
            .ok()?;
        Some((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    }

    #[test]
    fn a_line_for_the_host_reads_the_same_in_any_login_shell() {
        // ssh hands the line to the login shell as `$SHELL -c line`. Every
        // shell here, and fish where installed, runs the same thing: words
        // with quotes, `$`, `!`, backslashes and line breaks arrive as sent.
        let words = [
            "plain",
            "it's $HOME",
            "a!b",
            r"back\slash\",
            r"x\'y",
            "two\nlines",
            "-n",
            "%d 100%",
            "`date` $(date) ~ *",
            "caf\u{e9}",
        ];
        let mut line = "printf '[%s]\\n'".to_owned();
        for word in words {
            line.push(' ');
            line.push_str(&quote(word));
        }
        // The `\n` in printf's format, before a quote, sends this one
        // through the escapes; a line without one stays as it is.
        let encoded = for_any_shell(&line);
        assert!(encoded.contains("awk"), "{encoded}");
        let plain = for_any_shell("cd '/srv/app' && git 'status'");
        assert!(!plain.contains("awk"), "{plain}");
        let expected: String = words.iter().map(|w| format!("[{w}]\n")).collect();
        let mut ran = 0;
        for shell in [
            "/bin/sh",
            "/bin/dash",
            "/bin/bash",
            "/bin/zsh",
            "/bin/csh",
            "/bin/tcsh",
            "/opt/homebrew/bin/fish",
            "/usr/local/bin/fish",
            "/usr/bin/fish",
        ] {
            if !Path::new(shell).exists() {
                continue;
            }
            ran += 1;
            let said = run_in(shell, &encoded);
            assert_eq!(said, Some((true, expected.clone())), "{shell}");
            let said = run_in(shell, &format!("{plain}; echo done"));
            assert!(said.is_some_and(|(_, out)| out.contains("done")), "{shell}");
            // Quotes, `$` and backquotes alone stay readable, and arrive.
            let quoted = for_any_shell(&format!("printf '[%s]' {}", quote("it's $HOME `x` *")));
            assert!(!quoted.contains("awk"), "{quoted}");
            assert_eq!(
                run_in(shell, &quoted),
                Some((true, "[it's $HOME `x` *]".to_owned())),
                "{shell}"
            );
            // The terminal's line: the folder's `cd` is sh's, then the
            // login shell ($SHELL) runs the command there.
            let dir = std::env::temp_dir().join(format!("crc-any shell's !{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let here = std::fs::canonicalize(&dir).unwrap();
            let line = remote_command(&dir.to_string_lossy(), Some("pwd"));
            let said = run_in(shell, &line);
            let there = said
                .as_ref()
                .and_then(|(_, out)| std::fs::canonicalize(out.trim()).ok());
            let _ = std::fs::remove_dir_all(&dir);
            assert_eq!(there, Some(here), "{shell}: {said:?}");
        }
        assert!(ran >= 2, "no shells to run the lines in");
    }

    #[test]
    fn ssh_shares_one_connection_per_host() {
        let dir = Path::new("/home/someone/Library/Application Support/crc/ssh");
        let control = control_path(dir, "myhost").unwrap();
        assert!(control.starts_with(&format!("{}/cm-", dir.display())));
        assert_eq!(control_path(dir, "myhost"), Some(control.clone()));
        assert_ne!(control_path(dir, "other"), Some(control.clone()));
        assert_eq!(
            control_path(&Path::new("/x").join("y".repeat(90)), "h"),
            None
        );
        let args = ssh_args("myhost", "/srv/app", None, Some(&control));
        assert_eq!(args[0], "-t");
        assert_eq!(args[1..7].iter().step_by(2).collect::<Vec<_>>(), ["-o"; 3]);
        assert_eq!(args[4], format!("ControlPath=\"{control}\""));
        assert_eq!(
            args[7..],
            [
                "--".to_owned(),
                "myhost".into(),
                r#"sh -c 'cd '"'"'/srv/app'"'"' && exec "${SHELL:-/bin/sh}" -l'"#.into()
            ]
        );
        let args = ssh_args("myhost", "/srv/app", Some("claude"), None);
        assert_eq!(&args[..3], ["-t", "--", "myhost"]);
        // The destination is ssh's, after the options: parse finds it.
        assert_eq!(parse(&args[1..]).map(|t| t.host), Some("myhost".into()));
    }

    #[test]
    fn a_read_over_ssh_asks_nothing_and_shares_the_connection() {
        let line = "cd '/srv/app' && git 'status'";
        let args = batch_args("me@box", Some("/s/cm-1"), line);
        assert_eq!(
            args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=\"/s/cm-1\"",
                "-o",
                "ControlPersist=600",
                "--",
                "me@box",
                r#"sh -c 'cd '"'"'/srv/app'"'"' && git '"'"'status'"'"''"#,
            ]
        );
        // No terminal is asked for, and the line is one argument, last.
        assert!(!args.contains(&"-t".to_owned()));
        let args = batch_args("box", None, "true");
        assert_eq!(&args[8..], ["--", "box", "sh -c 'true'"]);
        assert_eq!(parse(&args).map(|t| t.host), Some("box".into()));
        assert_eq!(quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn a_change_over_ssh_asks_nothing_and_never_starts_the_shared_connection() {
        let line = "cd '/srv/app' && git 'push'";
        let args = write_args("me@box", Some("/s/cm-1"), line);
        assert_eq!(
            args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=\"/s/cm-1\"",
                "--",
                "me@box",
                r#"sh -c 'cd '"'"'/srv/app'"'"' && git '"'"'push'"'"''"#,
            ]
        );
        assert!(!args.iter().any(|a| a.starts_with("ControlPersist")));
        // No socket: a connection of its own, still never a master.
        let args = write_args("box", None, "true");
        assert_eq!(
            &args[8..],
            ["-o", "ControlMaster=no", "--", "box", "sh -c 'true'"]
        );
        assert_eq!(parse(&args).map(|t| t.host), Some("box".into()));
    }

    #[test]
    fn a_host_ssh_may_not_know_is_told_with_the_configs_hosts() {
        let config = "Host *\n  User me\nHost shots box.local\n  HostName 10.0.0.2\nHost=web !bad db?\nMatch all\nHost shots\n";
        let hosts = config_hosts(config);
        assert_eq!(hosts, ["shots", "box.local", "web"]);
        assert_eq!(unknown_host("shots", &hosts), None);
        assert_eq!(unknown_host("root@ubuntu-16gb-hel1", &hosts), None);
        assert_eq!(unknown_host("srv.example.com", &hosts), None);
        assert_eq!(
            unknown_host("ubuntu-16gb-hel1", &["shots".to_owned()]),
            Some("ubuntu-16gb-hel1 is not in ~/.ssh/config: did you mean shots?".to_owned())
        );
        assert_eq!(
            unknown_host("x", &hosts),
            Some(
                "x is not in ~/.ssh/config: did you mean one of shots, box.local, web?".to_owned()
            )
        );
        assert!(unknown_host("x", &[]).is_some_and(|s| s.contains("names no hosts")));
        let many: Vec<String> = (1..=7).map(|n| format!("h{n}")).collect();
        assert!(unknown_host("x", &many).is_some_and(|s| s.ends_with("h5, \u{2026}?")));
    }

    #[test]
    fn a_remote_terminal_whose_ssh_fails_stays_and_says_why() {
        // ssh's stand-in fails as ssh does when it cannot resolve the host.
        let dir = std::env::temp_dir().join(format!("crc-kept-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\necho \"ssh: Could not resolve hostname $2\" >&2\nexit 255\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = kept_open_args(
            &ssh,
            vec!["--".into(), "nohost".into()],
            "nohost",
            &["shots".to_owned()],
        );
        assert!(is_ssh_launch(Path::new("/bin/sh"), &args));
        let run = |status: i32, input: &str| {
            std::fs::write(
                &ssh,
                format!(
                    "#!/bin/sh\necho \"ssh: Could not resolve hostname $2\" >&2\nexit {status}\n"
                ),
            )
            .unwrap();
            use std::io::Write;
            let mut child = std::process::Command::new("/bin/sh")
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        };
        let out = run(255, "\n");
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(255));
        assert!(String::from_utf8_lossy(&out.stderr).contains("Could not resolve hostname nohost"));
        assert!(
            said.contains(
                "crc: could not connect to nohost. Check the host name; your ~/.ssh/config hosts are: shots"
            ),
            "{said}"
        );
        assert!(said.contains("Press Return to close this tab."), "{said}");
        // Its own command's status, without a word, for anything else.
        let out = run(1, "");
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(is_ssh_launch(Path::new("/usr/bin/ssh"), &[]));
        assert!(!is_ssh_launch(Path::new("/bin/zsh"), &["-l".to_owned()]));
    }

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_command_line_names_its_destination() {
        let t = parse(&words("root@62.238.118.239")).unwrap();
        assert_eq!(
            (t.user.as_deref(), t.host.as_str(), t.port),
            (Some("root"), "62.238.118.239", None)
        );
        let t = parse(&words("-p 2222 -i ~/.ssh/k -A box")).unwrap();
        assert_eq!((t.host.as_str(), t.port), ("box", Some(2222)));
        let t = parse(&words("-tt -l admin -oStrictHostKeyChecking=no db")).unwrap();
        assert_eq!((t.user.as_deref(), t.host.as_str()), (Some("admin"), "db"));
        let t = parse(&words("ssh://me@h.example:2200")).unwrap();
        assert_eq!(
            (t.user.as_deref(), t.host.as_str(), t.port),
            (Some("me"), "h.example", Some(2200))
        );
        assert_eq!(parse(&words("-V")), None);
    }

    #[test]
    fn an_alias_reads_its_config() {
        let config = "Host *\n  ServerAliveInterval 30\nHost other hetzner\n  HostName 62.238.118.239\n  User root\n  Port=2222\nHost next\n  HostName x\n";
        assert_eq!(
            from_config(config, "hetzner"),
            (
                Some("62.238.118.239".into()),
                Some("root".into()),
                Some(2222)
            )
        );
        assert_eq!(from_config(config, "nothing"), (None, None, None));
    }

    #[test]
    fn an_ssh_child_of_this_process_is_found() {
        // A stand-in named ssh: sleep under that name, a binary as ssh is.
        let dir = std::env::temp_dir().join(format!("crc-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ssh = dir.join("ssh");
        let _ = std::fs::remove_file(&ssh);
        std::os::unix::fs::symlink("/bin/sleep", &ssh).unwrap();
        let mut child = std::process::Command::new(&ssh).arg("5").spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let args = crate::platform::process_args(child.id() as i32);
        let found = of_shell(std::process::id() as i32);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(args.as_deref().map(|a| a.len()), Some(2), "{args:?}");
        assert_eq!(found.map(|t| t.destination), Some("5".into()));
    }
}

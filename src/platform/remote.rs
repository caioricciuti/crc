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

#[cfg(test)]
mod tests {
    use super::*;

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

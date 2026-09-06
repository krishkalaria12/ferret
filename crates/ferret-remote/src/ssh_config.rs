//! Importing hosts from `~/.ssh/config` (F-20).
//!
//! The rule this module is built around: a user's ssh config is *theirs*.
//! What comes out of here pre-fills the New Connection dialog and nothing
//! more — it is never a second, live source of connection settings, because
//! misreading one keyword must not silently change how Ferret connects.
//!
//! Aliases are found by scanning `Host` lines directly; everything else is
//! resolved through `ssh2-config`'s `query`, which applies the file's own
//! precedence and `Include` handling. Splitting it that way means an alias
//! whose parameters this crate cannot represent still appears in the list
//! rather than vanishing.

use crate::record::Connection;
use ssh2_config::{ParseRule, SshConfig};
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// The default location of the user's ssh config on both targets.
pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh").join("config"))
}

/// Every importable host in `path` (or the default file).
///
/// A missing or unreadable file is an empty list, not an error: import is an
/// offer, and a user with no ssh config has simply nothing to import.
pub fn parse(path: Option<&Path>) -> Vec<Connection> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => match default_path() {
            Some(path) => path,
            None => return Vec::new(),
        },
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    let Ok(config) = SshConfig::default().parse(
        &mut BufReader::new(text.as_bytes()),
        // Permissive on purpose. Strict parsing turns one unrecognised
        // keyword — of which OpenSSH has many that libssh2 never had — into
        // "you have no hosts", which is the worst possible answer.
        ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS,
    ) else {
        return Vec::new();
    };

    let mut imported = Vec::new();
    for alias in aliases(&text) {
        let params = config.query(&alias);
        // Two different questions with two different answers. The in-process
        // client opens a socket, so it needs the resolved address; `ssh` needs
        // the *alias*, because that is what matches the `Host` block carrying
        // the user's `ProxyJump`, `Match` and certificate settings. Storing
        // only the address would silently bypass all of it.
        let host = params.host_name.clone().unwrap_or_else(|| alias.clone());
        imported.push(Connection {
            name: alias.clone(),
            host,
            ssh_alias: Some(alias.clone()),
            user: params.user.clone().unwrap_or_default(),
            port: params.port,
            key_path: params
                .identity_file
                .as_ref()
                .and_then(|files| files.first().cloned()),
            notes: notes_for(&params),
            ..Default::default()
        });
    }
    imported
}

/// Host aliases, in file order, skipping patterns and negations.
///
/// Scanned here rather than read from `get_hosts()` so this does not depend on
/// the parser's internal clause representation — and so a malformed block
/// costs its own alias instead of the whole import.
fn aliases(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for pattern in parts {
            // `*`, `?` and `!` mean this block configures a class of hosts
            // rather than naming one, so there is nothing to import.
            if pattern.contains(['*', '?', '!']) {
                continue;
            }
            if !found.iter().any(|existing: &String| existing == pattern) {
                found.push(pattern.to_string());
            }
        }
    }
    found
}

/// A note recording anything Ferret's own client cannot honour.
///
/// `ProxyJump` is the case that matters: the interactive SSH terminal gets it
/// for free because it runs the user's own `ssh`, while in-process browsing
/// does not go through a jump host. Saying so on the record is honest; silently
/// connecting directly would not be.
fn notes_for(params: &ssh2_config::HostParams) -> String {
    match &params.proxy_jump {
        Some(jump) if !jump.is_empty() => format!(
            "Imported from ~/.ssh/config. The SSH terminal goes through ProxyJump {} \
             because it runs `ssh` against this alias; browsing connects directly and \
             will not reach a host that is only available through the jump.",
            jump.join(",")
        ),
        _ => "Imported from ~/.ssh/config.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# a comment
Host *
  ServerAliveInterval 60

Host build-box
  HostName 10.0.0.4
  User dorien
  Port 2222
  IdentityFile ~/.ssh/id_ed25519

Host jump-target
  HostName internal.example
  User ops
  ProxyJump bastion

Host web-* staging
  User deploy
";

    fn write(text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ferret-ssh-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn patterns_are_not_servers() {
        let names = aliases(SAMPLE);
        assert!(names.contains(&"build-box".to_string()));
        assert!(names.contains(&"staging".to_string()));
        // `*` and `web-*` configure a class of hosts; there is nothing to
        // connect to.
        assert!(!names.iter().any(|n| n.contains('*')));
    }

    #[test]
    fn a_host_block_becomes_a_record() {
        let path = write(SAMPLE);
        let imported = parse(Some(&path));
        std::fs::remove_file(&path).ok();

        let build = imported
            .iter()
            .find(|c| c.name == "build-box")
            .expect("build-box");
        assert_eq!(build.host, "10.0.0.4");
        assert_eq!(build.user, "dorien");
        assert_eq!(build.port, Some(2222));
        assert!(build.key_path.is_some());
    }

    #[test]
    fn an_imported_record_keeps_the_alias_ssh_needs() {
        let path = write(SAMPLE);
        let imported = parse(Some(&path));
        std::fs::remove_file(&path).ok();

        let jump = imported
            .iter()
            .find(|c| c.name == "jump-target")
            .expect("jump-target");
        // The client dials the resolved address...
        assert_eq!(jump.host, "internal.example");
        // ...and `ssh` is handed the alias, so the user's own Host block —
        // and the ProxyJump in it — actually applies.
        assert_eq!(jump.ssh_alias.as_deref(), Some("jump-target"));
        assert_eq!(jump.ssh_command(), "ssh jump-target");
    }

    #[test]
    fn a_jump_host_says_so_rather_than_pretending() {
        let path = write(SAMPLE);
        let imported = parse(Some(&path));
        std::fs::remove_file(&path).ok();

        let jump = imported
            .iter()
            .find(|c| c.name == "jump-target")
            .expect("jump-target");
        assert!(jump.notes.contains("ProxyJump"), "{}", jump.notes);
    }

    #[test]
    fn a_missing_file_imports_nothing_and_does_not_fail() {
        assert!(parse(Some(Path::new("/nonexistent/ssh/config"))).is_empty());
    }

    #[test]
    fn an_unrecognised_keyword_does_not_cost_the_whole_file() {
        // OpenSSH has keywords libssh2 never had. Strict parsing would turn
        // one of them into "you have no hosts".
        let path = write("Host box\n  HostName h\n  SomeFutureKeyword yes\n");
        let imported = parse(Some(&path));
        std::fs::remove_file(&path).ok();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].host, "h");
    }
}

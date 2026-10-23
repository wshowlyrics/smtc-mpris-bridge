//! `host.ini`: where to listen and which guest apps to publish.
//!
//! ```ini
//! [host]
//! listen = 192.168.122.1:47800
//!
//! [players]
//! allow = AppleMusicWin
//! ignore =
//! ```

use crate::names;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub struct Config {
    pub listen: SocketAddr,
    pub players: Players,
}

/// Which apps to publish. Entries are app ids or app names (`AppleMusicWin`).
/// Browsers are left out before either list applies ([`names::is_browser`]).
#[derive(Debug, Default, PartialEq)]
pub struct Players {
    /// Empty publishes every app.
    pub allow: Vec<String>,
    /// Applied after `allow`.
    pub ignore: Vec<String>,
}

impl Players {
    pub fn publishes(&self, aumid: &str) -> bool {
        let name = names::app_name(aumid);
        let matches = |list: &[String]| list.iter().any(|e| e == aumid || e == name);
        !names::is_browser(name)
            && (self.allow.is_empty() || matches(&self.allow))
            && !matches(&self.ignore)
    }
}

/// `allow A,B ignore -`, for the log.
impl std::fmt::Display for Players {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = |l: &[String]| {
            if l.is_empty() {
                "-".into()
            } else {
                l.join(",")
            }
        };
        write!(
            f,
            "allow {} ignore {}",
            list(&self.allow),
            list(&self.ignore)
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([192, 168, 122, 1], protocol::DEFAULT_PORT)),
            players: Players::default(),
        }
    }
}

/// An address with or without a port; the port defaults to 47800.
pub fn parse_listen(value: &str) -> Result<SocketAddr, String> {
    value
        .parse()
        .or_else(|_| {
            value
                .parse::<IpAddr>()
                .map(|ip| SocketAddr::new(ip, protocol::DEFAULT_PORT))
        })
        .map_err(|_| format!("not an address: {value}"))
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Sections and `key = value` lines; `#`/`;` start a comment line. Unknown
/// sections and keys are errors, so a typo does not silently publish everything.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut config = Config::default();
    let mut section = "";
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let at = |msg: String| format!("line {}: {msg}", i + 1);
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim();
            if !matches!(section, "host" | "players") {
                return Err(at(format!("unknown section [{section}]")));
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .map(|(k, v)| (k.trim(), v.trim()))
            .ok_or_else(|| at("expected key = value".into()))?;
        match (section, key) {
            ("host", "listen") => config.listen = parse_listen(value).map_err(at)?,
            ("players", "allow") => config.players.allow = list(value),
            ("players", "ignore") => config.players.ignore = list(value),
            _ => return Err(at(format!("unknown key {key} in [{section}]"))),
        }
    }
    Ok(config)
}

/// `$XDG_CONFIG_HOME/smtc-mpris-bridge/host.ini`, else under `~/.config`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("smtc-mpris-bridge").join("host.ini"))
}

/// A missing file is the defaults.
pub fn load(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLE: &str = "AppleInc.AppleMusicWin_nzyj5cx40ttqa!App";
    const SPOTIFY: &str = "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify";

    #[test]
    fn the_documented_example_parses() {
        let c = parse(
            "# comment\n[host]\nlisten = 10.0.0.1\n\n[players]\nallow = AppleMusicWin, SpotifyMusic\nignore =\n",
        )
        .unwrap();
        assert_eq!(c.listen.to_string(), "10.0.0.1:47800");
        assert_eq!(c.players.allow, ["AppleMusicWin", "SpotifyMusic"]);
        assert!(c.players.ignore.is_empty());
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn typos_are_errors_with_a_line_number() {
        assert_eq!(
            parse("[players]\nalow = x").unwrap_err(),
            "line 2: unknown key alow in [players]"
        );
        assert!(parse("[player]").unwrap_err().contains("unknown section"));
        assert!(parse("[host]\nlisten = virbr0")
            .unwrap_err()
            .starts_with("line 2"));
        assert!(parse("listen = 1.2.3.4").is_err()); // outside a section
    }

    #[test]
    fn allow_then_ignore() {
        let all = Players::default();
        assert!(all.publishes(APPLE) && all.publishes(SPOTIFY));

        let apple = Players {
            allow: vec!["AppleMusicWin".into()],
            ignore: vec![],
        };
        assert!(apple.publishes(APPLE) && !apple.publishes(SPOTIFY));

        let no_spotify = Players {
            allow: vec![],
            ignore: vec![SPOTIFY.into()],
        };
        assert!(no_spotify.publishes(APPLE) && !no_spotify.publishes(SPOTIFY));
    }

    #[test]
    fn browsers_are_never_published() {
        let edge = Players {
            allow: vec!["MSEdge".into()],
            ignore: vec![],
        };
        assert!(!edge.publishes("MSEdge"));
        assert!(!Players::default().publishes("Chrome"));
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        assert_eq!(
            load(Path::new("/nonexistent/host.ini")).unwrap(),
            Config::default()
        );
    }
}

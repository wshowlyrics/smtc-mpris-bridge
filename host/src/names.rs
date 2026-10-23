//! D-Bus names for guest players.

/// playerctl and wshowlyrics' `preferred_players` see every guest player as
/// an instance of `smtc_mpris_bridge`: both compare up to a `.` boundary.
const BUS_PREFIX: &str = "org.mpris.MediaPlayer2.smtc_mpris_bridge";

/// The app part of an SMTC `SourceAppUserModelId`:
/// `AppleInc.AppleMusicWin_nzyj5cx40ttqa!App` → `AppleMusicWin`. An unpackaged
/// app reports its exe (`Spotify.exe`) or a plain name, kept without `.exe`.
pub fn app_name(aumid: &str) -> &str {
    match aumid.split_once('!') {
        // Packaged: `<dotted name>_<publisher id>!<app id>`.
        Some((family, _)) => {
            let name = family.split_once('_').map_or(family, |(name, _)| name);
            name.rsplit('.').next().unwrap_or(name)
        }
        None => {
            let file = aumid.rsplit(['\\', '/']).next().unwrap_or(aumid);
            file.strip_suffix(".exe")
                .or_else(|| file.strip_suffix(".EXE"))
                .unwrap_or(file)
        }
    }
}

/// Browsers are never published, whatever `host.ini` says: wshowlyrics never
/// follows a browser (wshowlyrics/wshowlyrics#14 on GitHub), and its own check
/// cannot see a guest browser behind a `smtc_mpris_bridge.*` name. Same words
/// as its `is_browser_or_proxy`, but without case: Windows reports `MSEdge` and
/// `Chrome`.
pub fn is_browser(app: &str) -> bool {
    let app = app.to_ascii_lowercase();
    [
        "chrome", "chromium", "firefox", "vivaldi", "brave", "edge", "opera",
    ]
    .iter()
    .any(|b| app.contains(b))
}

/// One bus name element: `[A-Za-z0-9_]`, not starting with a digit. `-` is
/// legal but discouraged by the D-Bus spec, so it becomes `_` too.
fn element(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

pub fn bus_name(guest: &str, aumid: &str) -> String {
    format!(
        "{BUS_PREFIX}.{}.{}",
        element(guest),
        element(app_name(aumid))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_apps_use_the_package_name() {
        assert_eq!(
            app_name("AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"),
            "AppleMusicWin"
        );
        assert_eq!(
            app_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "SpotifyMusic"
        );
        assert_eq!(
            app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "ZuneMusic"
        );
    }

    #[test]
    fn unpackaged_apps_drop_the_exe() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(app_name(r"C:\Program Files\foo_bar.EXE"), "foo_bar");
        assert_eq!(app_name("Chrome"), "Chrome");
    }

    #[test]
    fn bus_names_are_valid_elements() {
        assert_eq!(
            bus_name(
                "DESKTOP-G2NP3G0",
                "AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"
            ),
            "org.mpris.MediaPlayer2.smtc_mpris_bridge.DESKTOP_G2NP3G0.AppleMusicWin"
        );
        assert_eq!(
            bus_name("1st vm", "x.exe"),
            "org.mpris.MediaPlayer2.smtc_mpris_bridge._1st_vm.x"
        );
        assert!(bus_name("", "").ends_with("._._"));
    }
}

//! Working-directory reports a shell sends in-band.
//!
//! Two conventions exist:
//! - OSC 7 `file://host/path`, percent-encoded (VTE, WezTerm, Ghostty,
//!   oh-my-posh, fish);
//! - OSC 9;9 `"path"`, a bare Windows path (ConEmu, adopted by Windows
//!   Terminal and suggested in its PowerShell and cmd setup guides).
//!
//! On Windows this is the only way to follow PowerShell: `Set-Location` does
//! not change the process working directory, so nothing outside the shell can
//! see it.

use std::path::PathBuf;

/// Parse the parameters of an OSC sequence as a working-directory report.
///
/// `local_host` is this machine's name: an OSC 7 report naming another host
/// comes from a remote shell (ssh) and says nothing about a local directory.
pub fn parse_osc_cwd(params: &[&[u8]], local_host: &str) -> Option<PathBuf> {
    // vte splits the payload on every ';', including those inside the path.
    let join = |rest: &[&[u8]]| {
        let parts: Vec<String> = rest
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        parts.join(";")
    };

    let path = match params {
        [b"7", rest @ ..] if !rest.is_empty() => parse_file_url(&join(rest), local_host)?,
        [b"9", b"9", rest @ ..] if !rest.is_empty() => {
            let raw = join(rest);
            let raw = raw.trim();
            raw.strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .unwrap_or(raw)
                .to_string()
        }
        _ => return None,
    };

    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(normalize_drive_path(path)))
}

/// `file://host/path` → the decoded path, when the host is this machine.
fn parse_file_url(url: &str, local_host: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => return None,
    };
    let is_local = host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case(local_host);
    if !is_local {
        return None;
    }
    let path = percent_decode(path)?;
    // `/C:/Users` carries a drive letter behind the URL's leading slash.
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(path[1..].to_string());
    }
    Some(path)
}

/// Spell a drive path (`C:/Users`) with the separator Windows displays.
fn normalize_drive_path(path: String) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        path.replace('/', "\\")
    } else {
        path
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(params: &[&str]) -> Option<PathBuf> {
        let params: Vec<&[u8]> = params.iter().map(|p| p.as_bytes()).collect();
        parse_osc_cwd(&params, "DESKTOP-1")
    }

    #[test]
    fn osc7_unix_path_is_decoded() {
        assert_eq!(
            parse(&["7", "file://localhost/home/me/my%20dir"]),
            Some(PathBuf::from("/home/me/my dir"))
        );
    }

    #[test]
    fn osc7_drive_path_drops_the_url_slash() {
        assert_eq!(
            parse(&["7", "file://desktop-1/C:/Users/me"]),
            Some(PathBuf::from(r"C:\Users\me"))
        );
    }

    #[test]
    fn osc7_from_another_host_is_ignored() {
        assert_eq!(parse(&["7", "file://server/home/me"]), None);
    }

    #[test]
    fn osc7_keeps_semicolons_in_the_path() {
        assert_eq!(
            parse(&["7", "file:///tmp/a", "b"]),
            Some(PathBuf::from("/tmp/a;b"))
        );
    }

    #[test]
    fn osc9_9_quoted_and_bare_paths() {
        assert_eq!(
            parse(&["9", "9", r#""D:\work""#]),
            Some(PathBuf::from(r"D:\work"))
        );
        assert_eq!(
            parse(&["9", "9", r"D:\work"]),
            Some(PathBuf::from(r"D:\work"))
        );
    }

    #[test]
    fn other_osc_sequences_are_not_reports() {
        assert_eq!(parse(&["0", "title"]), None);
        assert_eq!(parse(&["9", "notification"]), None);
        assert_eq!(parse(&["7"]), None);
        assert_eq!(parse(&["9", "9", ""]), None);
    }
}

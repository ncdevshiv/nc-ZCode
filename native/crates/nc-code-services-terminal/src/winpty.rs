//! Windows build number for `windowsPty.buildNumber`.
//!
//! The TypeScript service reads it from `os.release()` (`10.0.22631` → `22631`). Rust has
//! no equivalent std API, and `GetVersionExW` is manifest-dependent and reports a fake
//! `6.2` on current Windows, so the same number is taken from the authoritative registry
//! value `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\CurrentBuild`. When it cannot
//! be read the field is omitted, exactly like the TypeScript `undefined`.

/// `parseWindowsBuildNumber` — kept for the Node-style `major.minor.build` string.
pub fn parse_release_build_number(release_text: &str) -> Option<u32> {
    let build_text = release_text.split('.').nth(2)?;
    parse_build_number(build_text)
}

/// `Number.parseInt(text, 10)` restricted to a finite integer.
pub fn parse_build_number(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // parseInt stops at the first non-digit, matching the TypeScript behaviour.
    let digits: String = trimmed
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

#[cfg(windows)]
pub fn windows_build_number() -> Option<u32> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion")
        .ok()?;
    let current_build: String = key.get_value("CurrentBuild").ok()?;
    parse_build_number(&current_build)
}

#[cfg(not(windows))]
pub fn windows_build_number() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_third_release_component() {
        assert_eq!(parse_release_build_number("10.0.22631"), Some(22631));
        assert_eq!(parse_release_build_number("10.0.19045"), Some(19045));
        assert_eq!(parse_release_build_number("10.0"), None);
        assert_eq!(parse_release_build_number("10.0.x"), None);
        assert_eq!(parse_release_build_number("6.1.7601"), Some(7601));
    }

    #[test]
    fn parse_build_number_matches_parse_int_semantics() {
        assert_eq!(parse_build_number(" 26100 "), Some(26100));
        assert_eq!(parse_build_number("26100.1"), Some(26100));
        assert_eq!(parse_build_number(""), None);
        assert_eq!(parse_build_number("abc"), None);
    }

    #[cfg(windows)]
    #[test]
    fn reads_the_real_windows_build_from_the_registry() {
        // The real build number must be readable on this machine; if it is not, the implementation is wrong and we must not silently treat it as 0.
        let build = windows_build_number().expect("CurrentBuild must be readable");
        assert!(build > 10000, "unexpected build number: {build}");
    }
}

//! macOS terminal profile inheritance (`terminalProfileMacOs.ts`).
//!
//! iTerm2 and Terminal.app keep their profiles in plists; the TypeScript original shells
//! out to `plutil -convert json` and reads at most 2 MiB with a 2 second budget, treating
//! every failure as "no profile". The same best-effort rules apply here, and the *parsing*
//! is platform independent so it is unit-tested on every OS.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::profile::{normalize_font_family, normalize_font_size, DetectedProfile};
use crate::shell::{environment_get, Environment, Platform};

const PLIST_READ_TIMEOUT: Duration = Duration::from_millis(2_000);
const PLIST_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MONOSPACE_HINTS: [&str; 12] = [
    "mono",
    "code",
    "nerd",
    "powerline",
    "console",
    "menlo",
    "monaco",
    "courier",
    "cascadia",
    "consolas",
    "hack",
    // "meslo" is covered by the list above only for completeness of the TypeScript set.
    "meslo",
];

pub fn detect_iterm2(env: &Environment, platform: Platform, home_dir: &Path) -> DetectedProfile {
    let home = home_dir_for(env, platform, home_dir);
    let plist = read_macos_plist_file(
        &home
            .join("Library")
            .join("Preferences")
            .join("com.googlecode.iterm2.plist"),
        platform,
    );
    let Some(plist) = plist else {
        return DetectedProfile::default();
    };
    build_iterm2_profile(&plist)
}

/// `detectIterm2Profile` over an already converted plist.
pub fn build_iterm2_profile(plist: &Value) -> DetectedProfile {
    let profiles = plist
        .get("New Bookmarks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_default = |profile: &Value| {
        profile
            .get("Default Bookmark")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let default_profile = profiles.iter().find(|profile| is_default(profile));
    let ordered: Vec<Value> = match default_profile {
        Some(default) => std::iter::once(default.clone())
            .chain(
                profiles
                    .iter()
                    .filter(|profile| !is_default(profile))
                    .cloned(),
            )
            .collect(),
        None => profiles.clone(),
    };

    for profile in ordered {
        let descriptor = read_macos_font_descriptor(profile.get("Normal Font"));
        let theme = read_iterm2_theme(&profile);
        let candidate = DetectedProfile {
            font_family: descriptor.0,
            font_size: descriptor.1,
            theme,
        };
        if !candidate.font_family.is_none()
            || candidate.font_size.is_some()
            || candidate.theme.is_some()
        {
            return candidate;
        }
    }
    DetectedProfile::default()
}

pub fn detect_macos_terminal(
    env: &Environment,
    platform: Platform,
    home_dir: &Path,
) -> DetectedProfile {
    let home = home_dir_for(env, platform, home_dir);
    let plist = read_macos_plist_file(
        &home
            .join("Library")
            .join("Preferences")
            .join("com.apple.Terminal.plist"),
        platform,
    );
    let Some(plist) = plist else {
        return DetectedProfile::default();
    };
    build_macos_terminal_profile(&plist)
}

/// `detectMacOsTerminalProfile` over an already converted plist.
pub fn build_macos_terminal_profile(plist: &Value) -> DetectedProfile {
    let settings_names = ["Startup Window Settings", "Default Window Settings"]
        .iter()
        .filter_map(|key| plist.get(*key).and_then(Value::as_str).map(str::to_string))
        .collect::<Vec<String>>();

    for settings_name in settings_names {
        let Some(settings) = plist.get(&settings_name).filter(|value| value.is_object()) else {
            continue;
        };
        let font_family = settings
            .get("FontName")
            .and_then(Value::as_str)
            .and_then(|value| normalize_font_family(Some(value)))
            .or_else(|| {
                settings
                    .get("Font")
                    .and_then(Value::as_str)
                    .and_then(normalize_macos_font_name)
            })
            .or_else(|| read_macos_archived_font_name(settings.get("Font")));
        let font_size = normalize_font_size(settings.get("FontSize"))
            .or_else(|| read_macos_font_descriptor(settings.get("Font")).1);
        let candidate = DetectedProfile {
            font_family,
            font_size,
            theme: read_macos_terminal_theme(settings),
        };
        if candidate.font_family.is_some()
            || candidate.font_size.is_some()
            || candidate.theme.is_some()
        {
            return candidate;
        }
    }
    DetectedProfile::default()
}

fn home_dir_for(env: &Environment, platform: Platform, home_dir: &Path) -> PathBuf {
    environment_get(env, "HOME", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            environment_get(env, "USERPROFILE", platform)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| home_dir.to_path_buf())
}

/// `readMacOsPlistFile`: darwin only, `plutil` converts the plist to JSON.
pub fn read_macos_plist_file(path: &Path, platform: Platform) -> Option<Value> {
    if !platform.is_darwin() {
        return None;
    }
    if !path.is_file() {
        return None;
    }
    let output_file = temporary_plist_output(path)?;
    let status = run_plutil(path, &output_file)?;
    if !status.success() {
        std::fs::remove_file(&output_file).ok();
        return None;
    }
    let metadata = std::fs::metadata(&output_file).ok()?;
    if metadata.len() > PLIST_MAX_BYTES {
        std::fs::remove_file(&output_file).ok();
        return None;
    }
    let raw = std::fs::read_to_string(&output_file).ok()?;
    std::fs::remove_file(&output_file).ok();
    serde_json::from_str::<Value>(&raw)
        .ok()
        .filter(Value::is_object)
}

fn temporary_plist_output(path: &Path) -> Option<PathBuf> {
    let stamp = Instant::now()
        .elapsed()
        .as_nanos()
        .to_string()
        .chars()
        .last()
        .unwrap_or('0');
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "profile.plist".to_string());
    Some(std::env::temp_dir().join(format!(
        "zcode-plist-{}-{}-{stamp}.json",
        std::process::id(),
        name
    )))
}

fn run_plutil(path: &Path, output: &Path) -> Option<std::process::ExitStatus> {
    let file = std::fs::File::create(output).ok()?;
    let mut child = Command::new("plutil")
        .arg("-convert")
        .arg("json")
        .arg("-o")
        .arg("-")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if started.elapsed() > PLIST_READ_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// font descriptors

/// `normalizeMacOsFontName`: drop the trailing point size, then `-` becomes a space.
pub fn normalize_macos_font_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_size = trim_trailing_point_size(trimmed);
    let spaced = without_size.replace('-', " ");
    normalize_font_family(Some(&spaced))
}

fn trim_trailing_point_size(value: &str) -> String {
    let index = match value.rfind(' ') {
        Some(index) => index,
        None => return value.to_string(),
    };
    let tail = &value[index + 1..];
    let digits: String = tail
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    let is_plain_number = !digits.is_empty() && digits.len() == tail.len()
        || tail
            .chars()
            .filter(|character| character.is_ascii_digit() || *character == '.')
            .count()
            == tail.len()
            && tail.contains('.');
    if is_plain_number {
        return value[..index].to_string();
    }
    value.to_string()
}

/// `normalizeFontSize`: accept numbers or numeric strings inside the 6…72 range.
pub fn normalize_macos_font_size(value: &Value) -> Option<f64> {
    normalize_font_size(Some(value))
}

/// `readMacOsFontDescriptor` → `(family, size)`.
pub fn read_macos_font_descriptor(value: Option<&Value>) -> (Option<String>, Option<f64>) {
    let raw = match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(Value::Bool(flag)) => Some(flag.to_string()),
        _ => None,
    };
    let Some(raw) = raw else {
        return (None, None);
    };
    let font_family = normalize_macos_font_name(&raw);
    let font_size =
        trailing_point_size(&raw).and_then(|text| normalize_font_size(Some(&Value::String(text))));
    (font_family, font_size)
}

fn trailing_point_size(raw: &str) -> Option<String> {
    let index = raw.rfind(char::is_whitespace)?;
    let tail = raw[index + 1..].trim();
    if tail.is_empty() {
        return None;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    for character in tail.chars() {
        if character.is_ascii_digit() {
            saw_digit = true;
        } else if character == '.' {
            saw_dot = true;
        } else {
            return None;
        }
    }
    let _ = saw_dot;
    if !saw_digit {
        return None;
    }
    Some(tail.to_string())
}

/// `readMacOsArchivedFontName`: an archived `NS` base64 blob contains the font name.
pub fn read_macos_archived_font_name(value: Option<&Value>) -> Option<String> {
    use base64::Engine;

    let raw = match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(object)) => object.get("NS").map(|inner| match inner {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }),
        _ => None,
    }?;
    let normalized = normalize_font_family(Some(&raw))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(normalized.as_bytes())
        .ok()?;
    // Node's `toString("latin1")` maps every byte to U+0000…U+00FF.
    let latin1: String = decoded.iter().map(|byte| char::from(*byte)).collect();
    find_monospace_font_token(&latin1).and_then(|token| normalize_macos_font_name(&token))
}

/// The TypeScript `/([A-Za-z][A-Za-z0-9 ._-]*(?:Mono|Code|…)[A-Za-z0-9 ._-]*)/i`:
/// the leftmost run of allowed characters that contains one of the mono hints.
pub fn find_monospace_font_token(text: &str) -> Option<String> {
    let characters: Vec<char> = text.chars().collect();
    let is_allowed = |character: char| -> bool {
        character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '_' | '-')
    };
    let mut start = 0usize;
    while start < characters.len() {
        if !characters[start].is_ascii_alphabetic() {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < characters.len() && is_allowed(characters[end]) {
            end += 1;
        }
        let run: String = characters[start..end].iter().collect();
        let lowered = run.to_lowercase();
        if MONOSPACE_HINTS.iter().any(|hint| lowered.contains(hint)) {
            return Some(run);
        }
        start = end.max(start + 1);
    }
    None
}

// ---------------------------------------------------------------------------
// colours

fn color_record_value<'a>(record: &'a Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names.iter().filter_map(|name| record.get(*name)).next()
}

/// `normalizeColorComponent`: 0…1, 0…255 or 0…65535 scales.
pub fn normalize_color_component(value: Option<&Value>) -> Option<f64> {
    let parsed = match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !parsed.is_finite() {
        return None;
    }
    if (0.0..=1.0).contains(&parsed) {
        return Some(parsed);
    }
    if (0.0..=255.0).contains(&parsed) {
        return Some(parsed / 255.0);
    }
    if (0.0..=65_535.0).contains(&parsed) {
        return Some(parsed / 65_535.0);
    }
    None
}

/// `normalizeMacOsColor`: `#rrggbb`/`rgb()` strings pass through, colour dicts become hex.
pub fn normalize_macos_color(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => {
            let trimmed = text.trim();
            if is_css_color(trimmed) {
                Some(trimmed.to_string())
            } else {
                None
            }
        }
        Value::Object(record) => {
            let red = normalize_color_component(color_record_value(
                record,
                &["Red Component", "red", "Red"],
            ))?;
            let green = normalize_color_component(color_record_value(
                record,
                &["Green Component", "green", "Green"],
            ))?;
            let blue = normalize_color_component(color_record_value(
                record,
                &["Blue Component", "blue", "Blue"],
            ))?;
            let alpha = normalize_color_component(color_record_value(
                record,
                &["Alpha Component", "alpha", "Alpha", "Opacity"],
            ))
            .unwrap_or(1.0);
            let r = (red * 255.0).round();
            let g = (green * 255.0).round();
            let b = (blue * 255.0).round();
            if alpha < 1.0 {
                let rounded = (alpha * 1000.0).round() / 1000.0;
                return Some(format!(
                    "rgba({}, {}, {}, {rounded})",
                    r as u32, g as u32, b as u32
                ));
            }
            Some(format!(
                "#{r:02x}{g:02x}{b:02x}",
                r = r as u32,
                g = g as u32,
                b = b as u32
            ))
        }
        _ => None,
    }
}

fn is_css_color(value: &str) -> bool {
    if let Some(hex) = value.strip_prefix('#') {
        let rest = hex.to_lowercase();
        let valid = rest.chars().all(|character| character.is_ascii_hexdigit());
        return valid && matches!(rest.len(), 3 | 6 | 8) && !value.is_empty();
    }
    let lower = value.to_lowercase();
    lower.starts_with("rgb(") || lower.starts_with("rgba(")
}

fn read_theme_color(
    profile: &Value,
    theme: &mut Map<String, Value>,
    theme_key: &str,
    profile_keys: &[&str],
) {
    for profile_key in profile_keys {
        if let Some(color) = normalize_macos_color(profile.get(*profile_key)) {
            theme.insert(theme_key.to_string(), Value::String(color));
            return;
        }
    }
}

pub const ANSI_THEME_KEYS: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightBlack",
    "brightRed",
    "brightGreen",
    "brightYellow",
    "brightBlue",
    "brightMagenta",
    "brightCyan",
    "brightWhite",
];

/// `readIterm2Theme`, keeping the TypeScript insertion order.
pub fn read_iterm2_theme(profile: &Value) -> Option<Value> {
    let mut theme = Map::new();
    read_theme_color(profile, &mut theme, "foreground", &["Foreground Color"]);
    read_theme_color(profile, &mut theme, "background", &["Background Color"]);
    read_theme_color(profile, &mut theme, "cursor", &["Cursor Color"]);
    read_theme_color(profile, &mut theme, "cursorAccent", &["Cursor Text Color"]);
    read_theme_color(
        profile,
        &mut theme,
        "selectionBackground",
        &["Selection Color"],
    );
    for (index, key) in ANSI_THEME_KEYS.iter().enumerate() {
        read_theme_color(
            profile,
            &mut theme,
            key,
            &[
                &format!("Ansi {index} Color"),
                &format!("ANSI {index} Color"),
            ],
        );
    }
    compact_theme(theme)
}

/// `readMacOsTerminalTheme`, keeping the TypeScript insertion order.
pub fn read_macos_terminal_theme(profile: &Value) -> Option<Value> {
    let mut theme = Map::new();
    read_theme_color(profile, &mut theme, "foreground", &["TextColor"]);
    read_theme_color(profile, &mut theme, "background", &["BackgroundColor"]);
    read_theme_color(profile, &mut theme, "cursor", &["CursorColor"]);
    read_theme_color(
        profile,
        &mut theme,
        "selectionBackground",
        &["SelectionColor"],
    );
    let terminal_ansi_names: [&str; 16] = [
        "ANSIBlackColor",
        "ANSIRedColor",
        "ANSIGreenColor",
        "ANSIYellowColor",
        "ANSIBlueColor",
        "ANSIMagentaColor",
        "ANSICyanColor",
        "ANSIWhiteColor",
        "ANSIBrightBlackColor",
        "ANSIBrightRedColor",
        "ANSIBrightGreenColor",
        "ANSIBrightYellowColor",
        "ANSIBrightBlueColor",
        "ANSIBrightMagentaColor",
        "ANSIBrightCyanColor",
        "ANSIBrightWhiteColor",
    ];
    for (index, key) in ANSI_THEME_KEYS.iter().enumerate() {
        read_theme_color(profile, &mut theme, key, &[terminal_ansi_names[index]]);
    }
    compact_theme(theme)
}

fn compact_theme(theme: Map<String, Value>) -> Option<Value> {
    (!theme.is_empty()).then(|| Value::Object(theme))
}

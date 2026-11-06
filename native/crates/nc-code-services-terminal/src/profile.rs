//! Terminal font/theme profile inherited from the system terminal, mirroring
//! `packages/services/src/terminal/terminalProfile.ts`.
//!
//! The frontend renders the panel with these values, so the detector order, the JSONC
//! handling and the fallback font stack are ported rather than approximated. Everything
//! here is best-effort: a missing or unreadable config file simply yields no profile,
//! exactly like the TypeScript original.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::shell::{environment_get, Environment, Platform};
use crate::winpty::windows_build_number;

pub const FONT_FAMILY_FALLBACKS: [&str; 12] = [
    "ui-monospace",
    "SFMono-Regular",
    "SF Mono",
    "Menlo",
    "Monaco",
    "Consolas",
    "Cascadia Mono",
    "JetBrains Mono",
    "MesloLGS NF",
    "Hack Nerd Font",
    "Noto Sans Mono CJK SC",
    "monospace",
];

/// The two settings keys the profile resolution reads from `AppSettings`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalProfileSettings {
    pub terminal_font_family: Option<String>,
    pub terminal_inherit_system_profile: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetectedProfile {
    pub font_family: Option<String>,
    pub font_size: Option<f64>,
    pub theme: Option<Value>,
}

impl DetectedProfile {
    fn is_empty(&self) -> bool {
        self.font_family.is_none() && self.font_size.is_none() && self.theme.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontProfile {
    pub font_family: String,
    pub font_size: Option<f64>,
    pub theme: Option<Value>,
    pub source: &'static str,
}

/// `resolveTerminalFontProfile`: a custom family wins, then the detected system profile,
/// then the CSS fallback stack.
pub fn resolve_terminal_font_profile(
    settings: &TerminalProfileSettings,
    env: &Environment,
    platform: Platform,
    home_dir: &Path,
) -> FontProfile {
    let custom = normalize_font_family(settings.terminal_font_family.as_deref());
    let detected = if settings.terminal_inherit_system_profile != Some(false) {
        detect_system_terminal_profile(env, platform, home_dir)
    } else {
        None
    };

    if let Some(family) = custom {
        return FontProfile {
            font_family: dedupe_font_family_stack(&family),
            font_size: detected.as_ref().and_then(|profile| profile.font_size),
            theme: detected.and_then(|profile| profile.theme),
            source: "custom",
        };
    }
    if let Some(profile) = detected {
        return FontProfile {
            // macOS profiles can carry only colours; keep the stack usable then.
            font_family: dedupe_font_family_stack(
                profile
                    .font_family
                    .as_deref()
                    .unwrap_or(FONT_FAMILY_FALLBACKS[0]),
            ),
            font_size: profile.font_size,
            theme: profile.theme,
            source: "system",
        };
    }
    FontProfile {
        font_family: FONT_FAMILY_FALLBACKS.join(", "),
        font_size: None,
        theme: None,
        source: "fallback",
    }
}

/// `dedupeFontFamilyStack`: the primary family first, then every missing fallback once.
pub fn dedupe_font_family_stack(primary: &str) -> String {
    let mut stack: Vec<String> = primary
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    for fallback in FONT_FAMILY_FALLBACKS {
        if !stack.iter().any(|entry| entry == fallback) {
            stack.push(fallback.to_string());
        }
    }
    stack.join(", ")
}

pub fn normalize_font_family(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn normalize_font_size(value: Option<&Value>) -> Option<f64> {
    let parsed = match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !parsed.is_finite() || parsed < 6.0 || parsed > 72.0 {
        return None;
    }
    Some(parsed)
}

fn resolve_home_dir(env: &Environment, platform: Platform, home_dir: &Path) -> PathBuf {
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

/// One entry of `TERMINAL_FONT_DETECTORS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detector {
    WindowsTerminal,
    VsCode,
    ITerm2,
    MacOsTerminal,
    Kitty,
    Alacritty,
}

impl Detector {
    /// `platforms` in the TypeScript table; `None` means "run everywhere".
    pub fn platforms(self) -> Option<&'static [&'static str]> {
        match self {
            Self::WindowsTerminal => Some(&["win32"]),
            Self::VsCode => None,
            Self::ITerm2 | Self::MacOsTerminal => Some(&["darwin"]),
            Self::Kitty | Self::Alacritty => Some(&["darwin", "linux", "freebsd", "openbsd"]),
        }
    }

    pub fn applies_to(self, platform: Platform) -> bool {
        match self.platforms() {
            None => true,
            Some(platforms) => platforms.contains(&platform.as_node_str()),
        }
    }

    fn detect(self, env: &Environment, platform: Platform, home_dir: &Path) -> DetectedProfile {
        match self {
            Self::WindowsTerminal => detect_windows_terminal(env, platform, home_dir),
            Self::VsCode => detect_vscode(env, platform, home_dir),
            Self::ITerm2 => crate::profile_macos::detect_iterm2(env, platform, home_dir),
            Self::MacOsTerminal => {
                crate::profile_macos::detect_macos_terminal(env, platform, home_dir)
            }
            Self::Kitty => detect_kitty(env, platform, home_dir),
            Self::Alacritty => detect_alacritty(env, platform, home_dir),
        }
    }
}

/// The declaration order of `TERMINAL_FONT_DETECTORS`.
pub const DETECTORS: [Detector; 6] = [
    Detector::WindowsTerminal,
    Detector::VsCode,
    Detector::ITerm2,
    Detector::MacOsTerminal,
    Detector::Kitty,
    Detector::Alacritty,
];

/// `detectSystemTerminalProfile`: first non-empty detector wins.
pub fn detect_system_terminal_profile(
    env: &Environment,
    platform: Platform,
    home_dir: &Path,
) -> Option<DetectedProfile> {
    for detector in DETECTORS {
        if !detector.applies_to(platform) {
            continue;
        }
        let profile = detector.detect(env, platform, home_dir);
        if !profile.is_empty() {
            return Some(profile);
        }
    }
    None
}

fn read_nested_string(value: &Value, segments: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in segments {
        let object = current.as_object()?;
        current = object.get(*segment)?;
    }
    normalize_font_family(current.as_str())
}

fn read_object_file(path: &Path, parse: fn(&str) -> Option<Value>) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse(&raw)
}

/// Windows Terminal `settings.json` (JSONC) → `font.face` of the default profile.
pub fn detect_windows_terminal(
    env: &Environment,
    platform: Platform,
    _home_dir: &Path,
) -> DetectedProfile {
    if platform != Platform::Windows {
        return DetectedProfile::default();
    }
    let Some(local_app_data) = environment_get(env, "LOCALAPPDATA", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            environment_get(env, "APPDATA", platform)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    else {
        return DetectedProfile::default();
    };

    let candidates = [
        Path::new(local_app_data)
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json"),
        Path::new(local_app_data)
            .join("Packages")
            .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json"),
        Path::new(local_app_data)
            .join("Microsoft")
            .join("Windows Terminal")
            .join("settings.json"),
    ];

    for path in candidates {
        let Some(parsed) = read_object_file(&path, |raw| parse_jsonc(raw).map(Value::Object))
        else {
            continue;
        };
        let default_profile = read_nested_string(&parsed, &["defaultProfile"]);
        let profiles = parsed.get("profiles");
        // profiles is a {list, defaults} object, not an array (terminalProfile.ts:230-231).
        let list = profiles
            .and_then(|value| value.get("list"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if let Some(guid) = default_profile.as_deref() {
            for item in &list {
                if item.get("guid").and_then(Value::as_str) == Some(guid) {
                    if let Some(family) = read_nested_string(item, &["font", "face"]) {
                        return DetectedProfile {
                            font_family: Some(family),
                            ..Default::default()
                        };
                    }
                }
            }
        }

        if let Some(family) = read_nested_string(
            &profiles
                .and_then(|p| p.get("defaults").cloned())
                .unwrap_or(Value::Null),
            &["font", "face"],
        ) {
            return DetectedProfile {
                font_family: Some(family),
                ..Default::default()
            };
        }

        for item in &list {
            if let Some(family) = read_nested_string(item, &["font", "face"]) {
                return DetectedProfile {
                    font_family: Some(family),
                    ..Default::default()
                };
            }
        }
    }

    DetectedProfile::default()
}

/// VS Code `settings.json` → `terminal.integrated.fontFamily`.
pub fn detect_vscode(env: &Environment, platform: Platform, home_dir: &Path) -> DetectedProfile {
    let home = resolve_home_dir(env, platform, home_dir);
    let app_data = environment_get(env, "APPDATA", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let xdg_config = environment_get(env, "XDG_CONFIG_HOME", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut candidates: Vec<PathBuf> = Vec::new();
    for directory in [
        app_data.map(|value| PathBuf::from(value).join("Code")),
        app_data.map(|value| PathBuf::from(value).join("Code - Insiders")),
        xdg_config.map(|value| PathBuf::from(value).join("Code")),
        xdg_config.map(|value| PathBuf::from(value).join("Code - Insiders")),
        Some(home.join(".config").join("Code")),
        Some(home.join(".config").join("Code - Insiders")),
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Code"),
        ),
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Code - Insiders"),
        ),
    ]
    .into_iter()
    .flatten()
    {
        candidates.push(directory.join("User").join("settings.json"));
    }

    for path in candidates {
        let Some(parsed) = read_object_file(&path, |raw| parse_jsonc(raw).map(Value::Object))
        else {
            continue;
        };
        if let Some(family) = read_nested_string(&parsed, &["terminal.integrated.fontFamily"]) {
            return DetectedProfile {
                font_family: Some(family),
                ..Default::default()
            };
        }
    }
    DetectedProfile::default()
}

/// kitty.conf → `font_family` (last write wins is *not* the rule here: the TypeScript
/// regex returns the first match).
pub fn detect_kitty(env: &Environment, platform: Platform, home_dir: &Path) -> DetectedProfile {
    let home = resolve_home_dir(env, platform, home_dir);
    let xdg_config = environment_get(env, "XDG_CONFIG_HOME", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));

    let candidates = [
        xdg_config.join("kitty").join("kitty.conf"),
        home.join("Library")
            .join("Application Support")
            .join("kitty")
            .join("kitty.conf"),
    ];
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(family) = read_kitty_font_family(&raw) {
            return DetectedProfile {
                font_family: Some(family),
                ..Default::default()
            };
        }
    }
    DetectedProfile::default()
}

pub fn read_kitty_font_family(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("font_family") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) || rest.trim().is_empty() {
            continue;
        }
        let value = rest.trim();
        let value = value.strip_prefix('"').unwrap_or(value);
        let value = value.strip_suffix('"').unwrap_or(value);
        let value = normalize_font_family(Some(value));
        if value.is_some() {
            return value;
        }
    }
    None
}

/// alacritty config (TOML then YAML) → `font.normal.family`.
pub fn detect_alacritty(env: &Environment, platform: Platform, home_dir: &Path) -> DetectedProfile {
    let home = resolve_home_dir(env, platform, home_dir);
    let xdg_config = environment_get(env, "XDG_CONFIG_HOME", platform)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));

    for path in [
        xdg_config.join("alacritty").join("alacritty.toml"),
        home.join(".alacritty.toml"),
    ] {
        let Some(parsed) = read_object_file(&path, parse_toml) else {
            continue;
        };
        if let Some(family) = read_nested_string(&parsed, &["font", "normal", "family"]) {
            return DetectedProfile {
                font_family: Some(family),
                ..Default::default()
            };
        }
    }

    for path in [
        xdg_config.join("alacritty").join("alacritty.yml"),
        xdg_config.join("alacritty").join("alacritty.yaml"),
    ] {
        let Some(parsed) = read_object_file(&path, parse_yaml) else {
            continue;
        };
        if let Some(family) = read_nested_string(&parsed, &["font", "normal", "family"]) {
            return DetectedProfile {
                font_family: Some(family),
                ..Default::default()
            };
        }
    }
    DetectedProfile::default()
}

pub fn parse_toml(raw: &str) -> Option<Value> {
    let parsed: toml::Value = raw.parse().ok()?;
    let value = serde_json::to_value(parsed).ok()?;
    // The config reader only accepts a top-level object (an array/scalar has no keys to look up -> skip it).
    matches!(value, Value::Object(_)).then_some(value)
}

pub fn parse_yaml(raw: &str) -> Option<Value> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(raw).ok()?;
    let value = json_from_yaml(&parsed)?;
    // Same as above: a top-level array/scalar is not returned as a config object.
    matches!(value, Value::Object(_)).then_some(value)
}

fn json_from_yaml(value: &serde_yaml::Value) -> Option<Value> {
    use serde_yaml::Value as Y;
    match value {
        Y::Null => Some(Value::Null),
        Y::Bool(flag) => Some(Value::Bool(*flag)),
        Y::Number(number) => {
            if let Some(integer) = number.as_i64() {
                return Some(Value::from(integer));
            }
            number.as_f64().map(Value::from)
        }
        Y::String(text) => Some(Value::String(text.clone())),
        Y::Sequence(items) => Some(Value::Array(
            items.iter().filter_map(json_from_yaml).collect::<Vec<_>>(),
        )),
        Y::Mapping(mapping) => {
            let mut object = Map::new();
            for (key, item) in mapping {
                let text = match key {
                    Y::String(text) => text.clone(),
                    Y::Number(number) => number.to_string(),
                    Y::Bool(flag) => flag.to_string(),
                    _ => continue,
                };
                if let Some(value) = json_from_yaml(item) {
                    object.insert(text, value);
                }
            }
            Some(Value::Object(object))
        }
        Y::Tagged(_) => None,
    }
}

// ---------------------------------------------------------------------------
// JSONC (comments + trailing commas), ported from `stripJsonComments` and
// `removeJsonTrailingCommas`.

/// `stripJsonComments`: `//` line comments and `/* */` block comments are removed while
/// string literals (including escapes) are preserved.
pub fn strip_json_comments(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut result = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < chars.len() {
        let character = chars[index];
        let next = chars.get(index + 1).copied();
        if in_string {
            result.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                result.push(character);
                index += 1;
            }
            '/' if next == Some('/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
                result.push('\n');
            }
            '/' if next == Some('*') => {
                index += 2;
                while index < chars.len()
                    && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/'))
                {
                    index += 1;
                }
                // The loop stops at the '*': skip it together with the '/' (otherwise the slash would be left as an ordinary character).
                index += 2;
            }
            other => {
                result.push(other);
                index += 1;
            }
        }
    }
    result
}

/// `removeJsonTrailingCommas`: a comma directly before `}`/`]` (ignoring whitespace)
/// is dropped.
pub fn remove_json_trailing_commas(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut result = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < chars.len() {
        let character = chars[index];
        if in_string {
            result.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            in_string = true;
        }
        if character == ',' {
            let mut next_index = index + 1;
            while chars
                .get(next_index)
                .is_some_and(|value| value.is_whitespace())
            {
                next_index += 1;
            }
            if matches!(chars.get(next_index), Some('}') | Some(']')) {
                index += 1;
                continue;
            }
        }
        result.push(character);
        index += 1;
    }
    result
}

/// `parseJsonc`: try the raw text first, then the comment/comma-cleaned text.
pub fn parse_jsonc(raw: &str) -> Option<Map<String, Value>> {
    for candidate in [
        raw.to_string(),
        remove_json_trailing_commas(&strip_json_comments(raw)),
    ] {
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&candidate) {
            return Some(object);
        }
    }
    None
}

/// `TerminalWindowsPtyInfo`: node-pty reports the ConPTY backend and the Windows build
/// so the frontend can pick its resize behaviour.
pub fn windows_pty_info(platform: Platform) -> Option<Value> {
    if !platform.is_windows() {
        return None;
    }
    let mut object = Map::new();
    object.insert("backend".to_string(), Value::String("conpty".to_string()));
    if let Some(build_number) = windows_build_number() {
        object.insert("buildNumber".to_string(), Value::from(build_number));
    }
    Some(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn env(pairs: &[(&str, &str)]) -> Environment {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-terminal-profile-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        dir
    }

    #[test]
    fn strips_comments_outside_strings() {
        let raw = r#"{
  // line comment
  "face": "a // not a comment /* nor this */", /* block */
  "n": 1,
}"#;
        let stripped = strip_json_comments(raw);
        assert!(stripped.contains("\"a // not a comment /* nor this */\""));
        assert!(!stripped.contains("line comment"));
        let parsed = parse_jsonc(&remove_json_trailing_commas(&stripped)).expect("parses");
        assert_eq!(parsed["face"], json!("a // not a comment /* nor this */"));
        assert_eq!(parsed["n"], json!(1));
    }

    #[test]
    fn parse_jsonc_accepts_plain_json_and_rejects_arrays() {
        assert!(parse_jsonc(r#"{"a":1}"#).is_some());
        assert!(parse_jsonc(r#"[1,2]"#).is_none());
        assert!(parse_jsonc("not json at all").is_none());
    }

    #[test]
    fn remove_json_trailing_commas_keeps_string_content() {
        let raw = r#"{"list":[{"a":1,},], "text":"x,}"}"#;
        let cleaned = remove_json_trailing_commas(raw);
        assert_eq!(cleaned, r#"{"list":[{"a":1}], "text":"x,}"}"#);
    }

    #[test]
    fn dedupes_the_font_stack() {
        let stack = dedupe_font_family_stack("Fira Code, Menlo");
        assert!(stack.starts_with("Fira Code, Menlo, "));
        assert_eq!(stack.matches("Menlo").count(), 1, "no duplicate entries");
        assert!(stack.ends_with("monospace"));
        // TS dedupeFontFamilyStack places primary first, then fills the missing fallbacks (terminalProfile.ts:65-78).
        let expected = std::iter::once("monospace".to_string())
            .chain(
                FONT_FAMILY_FALLBACKS
                    .iter()
                    .filter(|entry| **entry != "monospace")
                    .map(|entry| entry.to_string()),
            )
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(dedupe_font_family_stack("monospace"), expected);
    }

    #[test]
    fn custom_family_wins() {
        let home = fixture_dir("custom-home");
        let settings = TerminalProfileSettings {
            terminal_font_family: Some("  Custom Mono  ".to_string()),
            terminal_inherit_system_profile: Some(true),
        };
        let profile = resolve_terminal_font_profile(&settings, &env(&[]), Platform::Linux, &home);
        assert_eq!(profile.source, "custom");
        assert!(profile.font_family.starts_with("Custom Mono,"));
    }

    #[test]
    fn fallback_profile_has_no_size_or_theme() {
        let home = fixture_dir("fallback-home");
        let settings = TerminalProfileSettings {
            terminal_font_family: None,
            terminal_inherit_system_profile: Some(false),
        };
        let profile = resolve_terminal_font_profile(&settings, &env(&[]), Platform::Linux, &home);
        assert_eq!(profile.source, "fallback");
        assert_eq!(profile.font_family, FONT_FAMILY_FALLBACKS.join(", "));
        assert!(profile.font_size.is_none());
        assert!(profile.theme.is_none());
    }

    #[test]
    fn windows_terminal_detector_reads_the_default_profile_font_face() {
        let local_app_data = fixture_dir("wt-localappdata");
        let settings_path = local_app_data
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState");
        std::fs::create_dir_all(&settings_path).expect("dirs");
        std::fs::write(
            settings_path.join("settings.json"),
            r#"{
  // Windows Terminal writes JSONC with comments and trailing commas.
  "defaultProfile": "{guid-second}",
  "profiles": {
    "list": [
      { "guid": "{guid-first}", "font": { "face": "Cascadia Code" } },
      { "guid": "{guid-second}", "font": { "face": "JetBrains Mono" } }
    ],
    "defaults": { "font": { "face": "Should Not Win" } },
  }
}"#,
        )
        .expect("settings.json");

        let env = env(&[("LOCALAPPDATA", local_app_data.to_str().expect("utf-8 path"))]);
        let profile = detect_windows_terminal(&env, Platform::Windows, Path::new("/"));
        assert_eq!(profile.font_family.as_deref(), Some("JetBrains Mono"));

        // The detector is win32 only, exactly like the TypeScript table.
        let off_windows = detect_windows_terminal(&env, Platform::Linux, Path::new("/"));
        assert!(off_windows.font_family.is_none());
        std::fs::remove_dir_all(local_app_data).ok();
    }

    #[test]
    fn windows_terminal_detector_falls_back_to_the_first_profile_with_a_font() {
        let local_app_data = fixture_dir("wt-fallback");
        let settings_path = local_app_data.join("Microsoft").join("Windows Terminal");
        std::fs::create_dir_all(&settings_path).expect("dirs");
        std::fs::write(
            settings_path.join("settings.json"),
            r#"{"profiles":{"list":[{"guid":"{a}"},{"guid":"{b}","font":{"face":"Consolas"}}],"defaults":{}}}"#,
        )
        .expect("settings.json");
        let env = env(&[("LOCALAPPDATA", local_app_data.to_str().expect("utf-8 path"))]);
        let profile = detect_windows_terminal(&env, Platform::Windows, Path::new("/"));
        assert_eq!(profile.font_family.as_deref(), Some("Consolas"));
        std::fs::remove_dir_all(local_app_data).ok();
    }

    #[test]
    fn vscode_detector_reads_the_integrated_font_family() {
        let xdg = fixture_dir("vscode-xdg");
        let user_dir = xdg.join("Code").join("User");
        std::fs::create_dir_all(&user_dir).expect("dirs");
        std::fs::write(
            user_dir.join("settings.json"),
            r#"{ "terminal.integrated.fontFamily": "Fira Mono", }"#,
        )
        .expect("settings.json");
        let env = env(&[("XDG_CONFIG_HOME", xdg.to_str().expect("utf-8"))]);
        let profile = detect_vscode(&env, Platform::Linux, &xdg);
        assert_eq!(profile.font_family.as_deref(), Some("Fira Mono"));
        std::fs::remove_dir_all(xdg).ok();
    }

    #[test]
    fn kitty_detector_reads_the_first_font_family_line() {
        assert_eq!(
            read_kitty_font_family("# font_family Ignored\nfont_family  Iosevka Nerd Font\n"),
            Some("Iosevka Nerd Font".to_string())
        );
        assert_eq!(
            read_kitty_font_family("font_family \"Monospace\""),
            Some("Monospace".to_string())
        );
        assert_eq!(read_kitty_font_family("font_family\n"), None);
        assert_eq!(read_kitty_font_family("# font_family x"), None);
    }

    #[test]
    fn alacritty_detector_prefers_toml_then_yaml() {
        let xdg = fixture_dir("alacritty");
        let config_dir = xdg.join("alacritty");
        std::fs::create_dir_all(&config_dir).expect("dirs");
        std::fs::write(
            config_dir.join("alacritty.toml"),
            "[font.normal]\nfamily = \"Humor Sans\"\n",
        )
        .expect("toml");
        let env = env(&[("XDG_CONFIG_HOME", xdg.to_str().expect("utf-8"))]);
        let profile = detect_alacritty(&env, Platform::Linux, &xdg);
        assert_eq!(profile.font_family.as_deref(), Some("Humor Sans"));

        std::fs::remove_file(config_dir.join("alacritty.toml")).expect("remove toml");
        std::fs::write(
            config_dir.join("alacritty.yml"),
            "font:\n  normal:\n    family: DejaVu Sans Mono\n",
        )
        .expect("yaml");
        let profile = detect_alacritty(&env, Platform::Linux, &xdg);
        assert_eq!(profile.font_family.as_deref(), Some("DejaVu Sans Mono"));
        std::fs::remove_dir_all(xdg).ok();
    }

    #[test]
    fn yaml_and_toml_parsers_return_objects_only() {
        assert_eq!(parse_yaml("a:\n  b: 1\n"), Some(json!({"a": {"b": 1}})));
        assert_eq!(
            parse_yaml("- 1\n- 2\n"),
            None,
            "top level arrays are skipped"
        );
        assert_eq!(
            parse_toml("[font.normal]\nfamily = \"X\"\n"),
            Some(json!({"font": {"normal": {"family": "X"}}}))
        );
        assert_eq!(parse_toml("=== broken"), None);
    }

    #[test]
    fn detector_platform_gates_match_the_typescript_table() {
        assert_eq!(Detector::WindowsTerminal.platforms(), Some(&["win32"][..]));
        assert_eq!(Detector::VsCode.platforms(), None);
        assert_eq!(Detector::ITerm2.platforms(), Some(&["darwin"][..]));
        assert_eq!(
            Detector::Alacritty.platforms(),
            Some(&["darwin", "linux", "freebsd", "openbsd"][..])
        );
        assert!(Detector::Kitty.applies_to(Platform::MacOS));
        assert!(!Detector::Kitty.applies_to(Platform::Windows));
        assert!(Detector::VsCode.applies_to(Platform::Linux));
        assert!(Detector::VsCode.applies_to(Platform::Windows));
        assert_eq!(
            DETECTORS,
            [
                Detector::WindowsTerminal,
                Detector::VsCode,
                Detector::ITerm2,
                Detector::MacOsTerminal,
                Detector::Kitty,
                Detector::Alacritty,
            ]
        );
    }

    #[test]
    fn system_profile_resolution_runs_only_platform_applicable_detectors() {
        let home = fixture_dir("system-home");
        let user_dir = home
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User");
        std::fs::create_dir_all(&user_dir).expect("dirs");
        std::fs::write(
            user_dir.join("settings.json"),
            r#"{"terminal.integrated.fontFamily":"Monaco"}"#,
        )
        .expect("settings");
        let env = env(&[("HOME", home.to_str().expect("utf-8"))]);
        let detected =
            detect_system_terminal_profile(&env, Platform::MacOS, &home).expect("vscode matches");
        assert_eq!(detected.font_family.as_deref(), Some("Monaco"));
        let profile = resolve_terminal_font_profile(
            &TerminalProfileSettings::default(),
            &env,
            Platform::MacOS,
            &home,
        );
        assert_eq!(profile.source, "system");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn font_size_normalization_clamps_like_the_typescript_helper() {
        assert_eq!(normalize_font_size(Some(&json!(11.0))), Some(11.0));
        assert_eq!(normalize_font_size(Some(&json!(" 13 "))), Some(13.0));
        assert_eq!(normalize_font_size(Some(&json!(5.0))), None);
        assert_eq!(normalize_font_size(Some(&json!(73.0))), None);
        assert_eq!(normalize_font_size(Some(&json!(null))), None);
        assert_eq!(normalize_font_size(None), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_pty_info_reports_conpty_with_the_real_build() {
        let info = windows_pty_info(Platform::Windows).expect("info on win32");
        assert_eq!(info["backend"], json!("conpty"));
        assert!(info["buildNumber"].as_u64().expect("build") > 10000);
        assert!(windows_pty_info(Platform::Linux).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_pty_info_is_absent_off_windows() {
        assert!(windows_pty_info(Platform::Windows).is_none());
        assert!(windows_pty_info(Platform::Linux).is_none());
    }
}

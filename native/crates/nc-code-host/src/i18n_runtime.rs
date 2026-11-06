//! i18n runtime business logic (ported from CLI).
//!
//! Business logic for locale detection and resolution separated from UI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupportedLocale {
    #[serde(rename = "en-US")]
    EnUs,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Default for SupportedLocale {
    fn default() -> Self {
        SupportedLocale::EnUs
    }
}

impl SupportedLocale {
    pub fn as_str(&self) -> &'static str {
        match self {
            SupportedLocale::EnUs => "en-US",
            SupportedLocale::ZhCn => "zh-CN",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocaleResolver;

impl LocaleResolver {
    pub fn new() -> Self {
        Self
    }

    pub fn is_supported(tag: &str) -> bool {
        matches!(tag, "en-US" | "zh-CN" | "en" | "zh")
    }

    pub fn normalize(tag: &str) -> Option<SupportedLocale> {
        let tag_lower = tag.to_lowercase();
        if tag_lower.starts_with("zh") {
            Some(SupportedLocale::ZhCn)
        } else if tag_lower.starts_with("en") {
            Some(SupportedLocale::EnUs)
        } else {
            None
        }
    }

    pub fn resolve(requested: Option<&str>, detected: Option<&str>) -> SupportedLocale {
        if let Some(req) = requested {
            if req != "auto" {
                if let Some(norm) = Self::normalize(req) {
                    return norm;
                }
            }
        }
        if let Some(det) = detected {
            if let Some(norm) = Self::normalize(det) {
                return norm;
            }
        }
        SupportedLocale::default()
    }
}

impl Default for LocaleResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_supported() {
        assert!(LocaleResolver::is_supported("en-US"));
        assert!(LocaleResolver::is_supported("zh-CN"));
        assert!(LocaleResolver::is_supported("en"));
        assert!(!LocaleResolver::is_supported("fr"));
    }

    #[test]
    fn test_normalize() {
        assert_eq!(
            LocaleResolver::normalize("zh-CN").unwrap(),
            SupportedLocale::ZhCn
        );
        assert_eq!(
            LocaleResolver::normalize("en_US").unwrap(),
            SupportedLocale::EnUs
        );
        assert_eq!(
            LocaleResolver::normalize("zh").unwrap(),
            SupportedLocale::ZhCn
        );
    }

    #[test]
    fn test_resolve() {
        assert_eq!(
            LocaleResolver::resolve(Some("zh-CN"), None),
            SupportedLocale::ZhCn
        );
        assert_eq!(
            LocaleResolver::resolve(Some("auto"), Some("en-US")),
            SupportedLocale::EnUs
        );
        assert_eq!(LocaleResolver::resolve(None, None), SupportedLocale::EnUs);
    }
}

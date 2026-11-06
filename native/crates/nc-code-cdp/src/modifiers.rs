//! The `modifiers` bit mask of the CDP Input domain.
//!
//! On the TS side `BrowserKeyModifier`/`BrowserPlaywrightModifier`
//! (`packages/shared/src/browser-use/commands.ts:24-30,185-192`) are handed to Playwright's
//! `keyboard.down/up` (`page-command.ts:37-49 withModifiers`); the raw CDP equivalent is the `modifiers`
//! bit field of `Input.dispatchMouseEvent`/`Input.dispatchKeyEvent`. The bit values are defined by the CDP
//! protocol itself (Alt=1, Ctrl=2, Meta=4, Shift=8), not invented by this crate.
//!
//! The platform normalization of `ControlOrMeta` mirrors the TS `normalizedModifier`
//! (`page-command.ts:29-35`): darwin → Meta, every other platform → Control.

use serde_json::Value;

/// The CDP `modifiers` bit field: Alt.
pub const MODIFIER_ALT: u32 = 1;
/// The CDP `modifiers` bit field: Ctrl.
pub const MODIFIER_CTRL: u32 = 2;
/// The CDP `modifiers` bit field: Meta (macOS Command).
pub const MODIFIER_META: u32 = 4;
/// The CDP `modifiers` bit field: Shift.
pub const MODIFIER_SHIFT: u32 = 8;

/// The platform normalization of `ControlOrMeta` (`page-command.ts:29-35`).
fn normalized(name: &str) -> &str {
    if name == "ControlOrMeta" {
        if cfg!(target_os = "macos") {
            "Meta"
        } else {
            "Control"
        }
    } else {
        name
    }
}

/// Fold the command's `modifiers` (`BrowserKeyModifier[]`, see commands.ts:24-30) into the CDP bit field.
/// Absent/not an array → 0 (no modifiers); invalid elements inside the array are ignored —— the strict
/// wire-level validation lives in the zod schemas of `packages/shared`, so this layer neither repeats it nor panics.
pub fn modifier_mask(value: Option<&Value>) -> u32 {
    let Some(items) = value.and_then(Value::as_array) else {
        return 0;
    };
    let mut mask = 0_u32;
    for item in items {
        let Some(name) = item.as_str() else { continue };
        mask |= match normalized(name) {
            "Alt" => MODIFIER_ALT,
            "Control" => MODIFIER_CTRL,
            "Meta" => MODIFIER_META,
            "Shift" => MODIFIER_SHIFT,
            _ => 0,
        };
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_or_non_array_yields_no_modifiers() {
        assert_eq!(modifier_mask(None), 0);
        assert_eq!(modifier_mask(Some(&json!("Alt"))), 0);
        assert_eq!(modifier_mask(Some(&json!({"Alt": true}))), 0);
    }

    #[test]
    fn each_modifier_maps_to_its_cdp_bit() {
        assert_eq!(modifier_mask(Some(&json!(["Alt"]))), MODIFIER_ALT);
        assert_eq!(modifier_mask(Some(&json!(["Control"]))), MODIFIER_CTRL);
        assert_eq!(modifier_mask(Some(&json!(["Meta"]))), MODIFIER_META);
        assert_eq!(modifier_mask(Some(&json!(["Shift"]))), MODIFIER_SHIFT);
    }

    #[test]
    fn multiple_modifiers_combine_disjointly() {
        // Shift+Alt = 8|1 = 9; repeated entries are idempotent under bitwise OR.
        assert_eq!(
            modifier_mask(Some(&json!(["Shift", "Alt", "Shift"]))),
            MODIFIER_SHIFT | MODIFIER_ALT
        );
    }

    #[test]
    fn control_or_meta_follows_the_platform_rule() {
        // page-command.ts:29-35: darwin → Meta, everything else → Control. Running this test on
        // Windows/Linux must yield the Ctrl bit; running it on macOS must yield the Meta bit.
        let mask = modifier_mask(Some(&json!(["ControlOrMeta"])));
        if cfg!(target_os = "macos") {
            assert_eq!(mask, MODIFIER_META);
        } else {
            assert_eq!(mask, MODIFIER_CTRL);
        }
    }
    #[test]
    fn unknown_and_non_string_entries_are_ignored() {
        assert_eq!(modifier_mask(Some(&json!(["Hyper", 7, null]))), 0);
        assert_eq!(
            modifier_mask(Some(&json!(["Hyper", "Control"]))),
            MODIFIER_CTRL,
            "Illegal items do not drag down legal items in the same array"
        );
    }
}

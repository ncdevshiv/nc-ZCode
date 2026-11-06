//! JS snippets and key mapping used by the shell's native browser-control port.
//!
//! The scripts run inside the child webview through `Runtime.evaluate`. They only
//! touch the page's own DOM (no `window.zcode`), and refs are ordinary
//! `data-zcode-ref` attributes so a later command can resolve them without a
//! second snapshot round-trip.

/// Assigns stable refs to actionable elements and returns a `BrowserSnapshot`
/// JSON string. `__MAX__` and `__INCLUDE_HIDDEN__` are substituted by Rust.
pub const SNAPSHOT_SCRIPT: &str = r#"(() => {
  const MAX = __MAX__;
  const INCLUDE_HIDDEN = __INCLUDE_HIDDEN__;
  document.querySelectorAll('[data-zcode-ref]').forEach((el) => el.removeAttribute('data-zcode-ref'));
  const candidates = Array.from(document.querySelectorAll(
    'a,button,input,textarea,select,summary,label,[role],[contenteditable="true"],[tabindex]'
  ));
  const isVisible = (el) => {
    const style = window.getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };
  const selectorFor = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const name = el.getAttribute('name');
    if (name) return el.tagName.toLowerCase() + '[name="' + CSS.escape(name) + '"]';
    const parts = [];
    let node = el;
    while (node && node.nodeType === 1 && parts.length < 6) {
      let part = node.tagName.toLowerCase();
      const parent = node.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter((child) => child.tagName === node.tagName);
        if (siblings.length > 1) part += ':nth-of-type(' + (siblings.indexOf(node) + 1) + ')';
      }
      parts.unshift(part);
      if (node.id) { parts[0] = '#' + CSS.escape(node.id); break; }
      node = parent;
    }
    return parts.join(' > ');
  };
  const elements = [];
  for (const el of candidates) {
    if (!INCLUDE_HIDDEN && !isVisible(el)) continue;
    const rect = el.getBoundingClientRect();
    const ref = 'e' + (elements.length + 1);
    el.setAttribute('data-zcode-ref', ref);
    const tag = el.tagName.toLowerCase();
    const text = (el.innerText || el.value || '').trim().slice(0, 300);
    elements.push({
      ref,
      tag,
      role: el.getAttribute('role') || undefined,
      name: (el.getAttribute('aria-label') || el.getAttribute('name') || '').slice(0, 200) || undefined,
      text: text || undefined,
      value: typeof el.value === 'string' ? el.value.slice(0, 300) : undefined,
      disabled: el.disabled === true || el.getAttribute('aria-disabled') === 'true',
      checked: typeof el.checked === 'boolean' ? el.checked : undefined,
      selector: selectorFor(el),
      xpath: '',
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
      inViewport: rect.top < window.innerHeight && rect.bottom > 0 && rect.left < window.innerWidth && rect.right > 0,
    });
    if (elements.length >= MAX) break;
  }
  return JSON.stringify({
    url: location.href,
    title: document.title,
    elements,
    truncated: elements.length >= MAX,
  });
})()"#;

/// Resolves a ref to a viewport point after scrolling it into view.
pub const REF_POINT_SCRIPT: &str = r#"(() => {
  const el = document.querySelector('[data-zcode-ref="__REF__"]');
  if (!el) return JSON.stringify({ ok: false, reason: 'ref_not_found' });
  el.scrollIntoView({ block: 'center', inline: 'center' });
  const rect = el.getBoundingClientRect();
  return JSON.stringify({ ok: true, x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 });
})()"#;

/// Focuses a ref, sets its value through the native setter and fires the input
/// events frameworks listen to.
pub const FILL_SCRIPT: &str = r#"(() => {
  const el = document.querySelector('[data-zcode-ref="__REF__"]');
  if (!el) return JSON.stringify({ ok: false, reason: 'ref_not_found' });
  el.focus();
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
    : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype
    : HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
  if (setter) setter.call(el, __VALUE__); else el.value = __VALUE__;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return JSON.stringify({ ok: true });
})()"#;

/// Focuses the element addressed by a ref (or the body when absent).
pub const FOCUS_SCRIPT: &str = r#"(() => {
  const ref = __REF__;
  const el = ref ? document.querySelector('[data-zcode-ref="' + ref + '"]') : document.body;
  if (el && typeof el.focus === 'function') el.focus();
  return JSON.stringify({ ok: Boolean(el) });
})()"#;

/// Page state: URL, title, scroll and viewport. History flags come from the
/// shell registry, not the page.
pub const STATE_SCRIPT: &str = r#"JSON.stringify({
  url: location.href,
  title: document.title,
  scrollX: window.scrollX,
  scrollY: window.scrollY,
  viewportWidth: window.innerWidth,
  viewportHeight: window.innerHeight
})"#;

/// True when the document finished loading.
pub const READY_SCRIPT: &str = "document.readyState === 'complete'";

/// Selector/text probe used by `waitFor`.
pub const WAIT_SCRIPT: &str = r#"(() => {
  const selector = __SELECTOR__;
  const text = __TEXT__;
  const textGone = __TEXT_GONE__;
  const bodyText = document.body ? document.body.innerText : '';
  return JSON.stringify({
    selector: selector ? Boolean(document.querySelector(selector)) : true,
    text: text ? bodyText.includes(text) : true,
    textGone: textGone ? !bodyText.includes(textGone) : true,
  });
})()"#;

/// CDP key fields for the keys the tool accepts. Returns `(code, virtual_key)`
/// and the key name to send.
pub fn key_fields(key: &str) -> Option<(String, i32, String)> {
    let normalized = key.trim();
    let mapped = match normalized {
        "Enter" | "Return" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" | "Esc" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" | "Del" => ("Delete", 46),
        "ArrowUp" | "Up" => ("ArrowUp", 38),
        "ArrowDown" | "Down" => ("ArrowDown", 40),
        "ArrowLeft" | "Left" => ("ArrowLeft", 37),
        "ArrowRight" | "Right" => ("ArrowRight", 39),
        "Home" => ("Home", 36),
        "End" => ("End", 35),
        "PageUp" => ("PageUp", 33),
        "PageDown" => ("PageDown", 34),
        " " | "Space" => ("Space", 32),
        _ => {
            let mut chars = normalized.chars();
            let first = chars.next()?;
            if chars.next().is_some() || !first.is_ascii_alphanumeric() {
                return None;
            }
            let code = first.to_ascii_uppercase();
            let virtual_key = code as i32;
            return Some((code.to_string(), virtual_key, normalized.to_string()));
        }
    };
    Some((mapped.0.to_string(), mapped.1, normalized.to_string()))
}

/// Escape a value for interpolation into a single-quoted JS string literal.
pub fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// Refs are generated as `e<number>`; anything else is rejected before it is
/// interpolated into a selector.
pub fn is_valid_ref(value: &str) -> bool {
    let mut chars = value.chars();
    if chars.next() != Some('e') {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    !rest.is_empty() && rest.len() <= 9 && rest.iter().all(char::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_named_and_character_keys() {
        assert_eq!(
            key_fields("Enter"),
            Some(("Enter".to_string(), 13, "Enter".to_string()))
        );
        assert_eq!(
            key_fields("ArrowLeft"),
            Some(("ArrowLeft".to_string(), 37, "ArrowLeft".to_string()))
        );
        assert_eq!(
            key_fields("a"),
            Some(("A".to_string(), 65, "a".to_string()))
        );
        assert_eq!(
            key_fields("7"),
            Some(("7".to_string(), 55, "7".to_string()))
        );
        assert_eq!(key_fields("F13"), None);
        assert_eq!(key_fields(""), None);
        assert_eq!(key_fields("ab"), None);
    }

    #[test]
    fn validates_generated_refs_only() {
        assert!(is_valid_ref("e1"));
        assert!(is_valid_ref("e123456789"));
        assert!(!is_valid_ref("e"));
        assert!(!is_valid_ref("e1234567890"));
        assert!(!is_valid_ref("x1"));
        assert!(!is_valid_ref("e1\""));
        assert!(!is_valid_ref(""));
    }

    #[test]
    fn escapes_js_strings_with_json_rules() {
        assert_eq!(js_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(js_string("line\nbreak"), "\"line\\nbreak\"");
    }

    #[test]
    fn scripts_keep_their_placeholders() {
        for placeholder in ["__MAX__", "__INCLUDE_HIDDEN__"] {
            assert!(SNAPSHOT_SCRIPT.contains(placeholder));
        }
        for placeholder in ["__REF__", "__VALUE__"] {
            assert!(FILL_SCRIPT.contains(placeholder));
        }
        assert!(WAIT_SCRIPT.contains("__SELECTOR__"));
        assert!(WAIT_SCRIPT.contains("__TEXT_GONE__"));
    }
}

//! Snapshot equivalent layer: map the AX tree of CDP `Accessibility.getFullAXTree` to TS
//! `packages/shared/src/browser-use/snapshot.ts` definition,
//! The `BrowserSnapshot` shape produced by `apps/nc-code-cli/packages/adapters/src/browser/snapshot.ts`.
//!
//! ## Relationship with TS reference implementation (be sure to read first, the honest boundary of parity)
//! TS reference implementation (adapters/src/browser/snapshot.ts:16-210 `collectSnapshot`) is actually
//! **DOM walkthrough** (`querySelectorAll` hit collection via getComputedStyle and getBoundingClientRect
//! for visibility/box model), not an AX tree; R3 explicitly requires the Rust side to use it instead
//! `Accessibility.getFullAXTree`** (specs/rust-pure-native-roadmap.md §7 R3). The two are not the same
//! Data source, so "equivalence" here means that the output schema is equivalent to the field/sorting semantics, rather than byte-by-byte equality
//! DOM walkthrough results:
//!
//! - Output structure field-by-field mirroring Zod schema (field name, optionality, strict, order) of `snapshot.ts` (TS) -
//!   See the `BrowserSnapshot*` serde structure of this document.
//! - ref semantic equivalent: allocate `e1, e2, …` in DOM order (TS snapshot.ts:122 `ref = 'e${index+1}'`),
//!   This layer is reproduced using the DFS document order of the AX tree root→childIds (ax DOM order == document order).
//! - role/name semantic equivalence: TS uses `implicitRole` (snapshot.ts:41-53) with `accessibleName`
//!   (snapshot.ts:54-63, priority aria-label|alt|title|placeholder|innerText|textContent).
//!   The AX tree directly gives the **calculated** role and accessible name, which is a superset (more accurate) of the local machine, so this layer takes
//!   `node.role.value` / `node.name.value`.
//! - Text normalization equivalent: TS `text()` (snapshot.ts:36-40) does `trim` + `\s+→' '` + slice; this layer
//!   `normalize_text` reproduces character by character.
//! - rect/selector/xpath/tag: These are DOM facts, not the AX tree. Use DOM fields by the page layer
//!   (`DOM.getBoxModel` takes quad→rect, `DOM.describeNode` takes tag, `DOM.getDocument` tree calculation
//!   selector/xpath) is parsed and injected through `Geometry`; when missing, rect falls to 0 and selector/xpath falls to an empty string.
//!   (TS schema has no min constraints on selector/xpath, see snapshot.ts:28-29).
//! - truncated/domTruncated: TS `actionNodes.length > maxElements` (snapshot.ts:205)/
//!   `domNodes.length > maxDomNodes` (snapshot.ts:207); the same formula for this layer.
//!
//! Known differences (documented, not missing): `inViewport` requires viewport size + per-node rect; AX tree only gives
//! `backendDOMNodeId`, so `inViewport` relies on the rect parsed by the page layer (press false when not parsed). TS's//! `framePath` (iframe penetration, snapshot.ts:34-35) This layer does not produce (raw CDP frame-by-frame AX tree needs to be
//! The frameId is adjusted separately to getFullAXTree, and the wiring is supplemented in the future).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Consistent with TS `DEFAULT_MAX_ELEMENTS = 200` (snapshot.ts:4).
pub const DEFAULT_MAX_ELEMENTS: usize = 200;
/// Consistent with TS `DEFAULT_MAX_DOM_NODES = 300` (snapshot.ts:5).
pub const DEFAULT_MAX_DOM_NODES: usize = 300;
/// Consistent with the 120 limit of TS `accessibleName` (snapshot.ts:62).
const NAME_MAX: usize = 120;
/// Consistent with the upper limit of 100 for TS element `text` (snapshot.ts:130).
const ELEMENT_TEXT_MAX: usize = 100;
/// Consistent with the upper limit of 300 for TS dom `semanticText` (snapshot.ts:176).
const DOM_TEXT_MAX: usize = 300;
/// Consistent with the upper limit of 240 for TS `value`/`attributes` (snapshot.ts:81,132).
const VALUE_MAX: usize = 240;

/// Actionable role set: AX role nodes falling into this set enter `elements` (TS relies on actionSelector
/// `a[href],button,input,textarea,select,[role],[onclick],[tabindex],summary,label,[contenteditable]`
/// Select interactive elements, snapshot.ts:17-18,119-121; use role/focusable/editable in the AX tree to express the same intention).
const ACTIONABLE_ROLES: &[&str] = &[
    "link",
    "button",
    "textbox",
    "searchbox",
    "combobox",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "tab",
    "link",
];

/// Semantic DOM tag set (tags covered by TS domSelector, snapshot.ts:19-20); AX nodes are approximated by role,
/// This collection is used to include roles with "content semantics" into the `dom` array.
const SEMANTIC_ROLES: &[&str] = &[
    "heading",
    "paragraph",
    "list",
    "listitem",
    "term",
    "definition",
    "blockquote",
    "code",
    "table",
    "row",
    "cell",
    "columnheader",
    "rowheader",
    "form",
    "image",
    "figure",
    "banner",
    "navigation",
    "contentinfo",
    "complementary",
    "region",
    "main",
    "article",
];
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserElementRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshotElement {
    /// The TS field name is `ref` (snapshot.ts:24); ref is a keyword in Rust, using the original identifier.
    pub r#ref: String,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    pub selector: String,
    pub xpath: String,
    pub rect: BrowserElementRect,
    pub in_viewport: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshotDomNode {
    pub tag: String,
    pub depth: i64,
    pub in_viewport: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshot {
    /// `dom` must come before `elements`: TS schema annotation (snapshot.ts:64-69) explains when large pages are truncated
    /// The model should read the page semantics first. serde serializes in declaration order, maintaining the same order.
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom: Option<Vec<BrowserSnapshotDomNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom_truncated: Option<bool>,
    pub elements: Vec<BrowserSnapshotElement>,
    pub truncated: bool,
}

/// Per-node geometry of the page layer parsed through the DOM domain; key = AX `backendDOMNodeId`.
#[derive(Debug, Clone, Default)]
pub struct ResolvedGeometry {
    pub tag: String,
    pub rect: BrowserElementRect,
    pub selector: String,
    pub xpath: String,
    pub in_viewport: bool,
}

pub type Geometry = HashMap<i64, ResolvedGeometry>;

#[derive(Debug, Clone, Copy)]
pub struct SnapshotOptions {
    pub max_elements: usize,
    pub max_dom_nodes: usize,
    pub include_hidden: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            max_elements: DEFAULT_MAX_ELEMENTS,
            max_dom_nodes: DEFAULT_MAX_DOM_NODES,
            include_hidden: false,
        }
    }
}

/// The original AX node of `Accessibility.getFullAXTree` (only the fields used by this layer are taken).
#[derive(Debug, Clone)]
pub struct RawAxNode {
    pub node_id: String,
    pub ignored: bool,
    pub role: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
    pub description: Option<String>,
    pub focusable: bool,
    pub editable: bool,
    pub disabled: bool,
    /// AX `checked` properties: true/false/mixed/undefined.
    pub checked: Option<bool>,
    pub child_ids: Vec<String>,
    pub backend_dom_node_id: Option<i64>,
}

impl RawAxNode {
    pub fn actionable(&self) -> bool {
        // Mirror actionSelector's intention: explicit role hit, focusable ([tabindex]/native focusable),
        // Editable ([contenteditable]/input) are all actionable handles.
        if let Some(role) = &self.role {
            if ACTIONABLE_ROLES.contains(&role.as_str()) {
                return true;
            }
        }
        self.focusable || self.editable
    }

    pub fn semantic(&self) -> bool {
        self.role
            .as_deref()
            .is_some_and(|role| SEMANTIC_ROLES.contains(&role))
    }

    fn hidden(&self) -> bool {
        // The ignored node of AX is equivalent to the display:none/visibility:hidden of TS (not included in the snapshot unless
        //includeHidden). Only ignored is mapped here; fine-grained opacity/0 size is determined by rect (page layer).
        self.ignored
    }
}

/// Parse the result of `Accessibility.getFullAXTree` (`{nodes:[...]}`) into a list of `RawAxNode`.
pub fn parse_ax_tree(result: &Value) -> Vec<RawAxNode> {
    let Some(nodes) = result.get("nodes").and_then(Value::as_array) else {
        return Vec::new();
    };
    nodes
        .iter()
        .map(|node| {
            let ax_str = |field: &str| {
                node.get(field)
                    .and_then(|v| v.get("value"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            };
            let prop_bool = |name: &str| -> Option<bool> {
                node.get("properties")
                    .and_then(Value::as_array)
                    .and_then(|props| {
                        props.iter().find_map(|p| {
                            if p.get("name").and_then(Value::as_str) == Some(name) {
                                p.get("value").and_then(|v| {
                                    v.get("value")
                                        .and_then(Value::as_bool)
                                        .or_else(|| v.as_bool())
                                })
                            } else {
                                None
                            }
                        })
                    })
            };
            let top_bool = |name: &str| node.get(name).and_then(Value::as_bool);
            RawAxNode {
                node_id: node
                    .get("nodeId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                ignored: top_bool("ignored").unwrap_or(false),
                role: ax_str("role"),
                name: ax_str("name"),
                value: ax_str("value"),
                description: ax_str("description"),
                focusable: prop_bool("focusable")
                    .or_else(|| top_bool("focusable"))
                    .unwrap_or(false),
                editable: prop_bool("editable")
                    .or_else(|| top_bool("editable"))
                    .unwrap_or(false),
                disabled: prop_bool("disabled")
                    .or_else(|| top_bool("disabled"))
                    .unwrap_or(false),
                checked: prop_bool("checked"),
                child_ids: node
                    .get("childIds")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                backend_dom_node_id: node.get("backendDOMNodeId").and_then(Value::as_i64),
            }
        })
        .collect()
}

/// Text normalization: Reprint TS `text(value, max)` (snapshot.ts:36-40) - trim, `\\s+→' '`, truncate by char.
pub fn normalize_text(raw: &str, max: usize) -> String {
    let collapsed: String = raw
        .trim()
        .chars()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(max).collect()
}

/// Starting from root, press childIds DFS to return the non-reentrant node sequence in document order (== DOM order).
/// Loops/self-references are protected with the visited set.
pub fn ax_nodes_in_dom_order<'a>(
    nodes: &'a [RawAxNode],
    root_id: Option<&str>,
) -> Vec<&'a RawAxNode> {
    let mut by_id: HashMap<&str, &RawAxNode> = HashMap::new();
    for node in nodes {
        by_id.insert(node.node_id.as_str(), node);
    }
    let root = root_id
        .and_then(|id| by_id.get(id).copied())
        .or_else(|| nodes.first())
        .map(|n| n.node_id.as_str());
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut stack = Vec::new();
    if let Some(root) = root {
        stack.push(root);
    } else {
        for n in nodes {
            stack.push(n.node_id.as_str());
        }
    }
    // Iterate over DFS (preorder), keeping childIds order → document order.
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(node) = by_id.get(id).copied() {
            out.push(node);
            for child in node.child_ids.iter().rev() {
                stack.push(child.as_str());
            }
        }
    }
    // Bottom line: add nodes that are not in the root tree (such as multiple roots/disconnects) in the original order to ensure that no actionable elements are missed.
    for node in nodes {
        if visited.insert(node.node_id.as_str()) {
            out.push(node);
        }
    }
    out
}

/// Build a snapshot. `ordered` must be the output (document order) of `ax_nodes_in_dom_order`.
/// Aligned with TS semantics:
/// - `max_elements` before elements = actionable and (include_hidden or not hidden), ref=e1..
///   (snapshot.ts:119-123); `truncated` = total number of actionable candidates > max_elements (snapshot.ts:205).
/// - dom = (actionable or semantic) and visible first `max_dom_nodes` (snapshot.ts:164-167),
///   `depth` = the ancestor depth to body, here approximated by the level to the root in the AX tree (the depth starting point of body==root).
pub fn build_snapshot(
    ordered: &[&RawAxNode],
    url: &str,
    title: &str,
    geometry: &Geometry,
    options: &SnapshotOptions,
) -> BrowserSnapshot {
    let max_elements = options.max_elements.max(1);
    let actionable: Vec<&&RawAxNode> = ordered
        .iter()
        .filter(|n| n.actionable() && (options.include_hidden || !n.hidden()))
        .collect();

    // parentRef: the nearest actionable ancestor to which ref has been assigned. Approximately using document order + nodeId index:
    // Record the mapping of each node to the nearest actionable ancestor (requires parent relationship, inferred from childIds).
    let parent_of = build_parent_map(ordered);

    let mut ref_by_node: HashMap<&str, String> = HashMap::new();
    let mut elements: Vec<BrowserSnapshotElement> = Vec::new();
    for (index, node) in actionable.iter().enumerate().take(max_elements) {
        let rf = format!("e{}", index + 1);
        ref_by_node.insert(node.node_id.as_str(), rf.clone());
        elements.push(build_element(node, &rf, &ref_by_node, &parent_of, geometry));
    }

    // dom array: actionable ∪ semantic, document order, with ref (only if it also belongs to elements).
    let dom_candidates: Vec<&&RawAxNode> = ordered
        .iter()
        .filter(|n| (n.actionable() || n.semantic()) && (options.include_hidden || !n.hidden()))
        .collect();
    let depths = compute_depths(ordered);
    let dom_truncated = dom_candidates.len() > options.max_dom_nodes;
    let dom: Vec<BrowserSnapshotDomNode> = dom_candidates
        .iter()
        .take(options.max_dom_nodes)
        .map(|node| {
            let geo = node.backend_dom_node_id.and_then(|id| geometry.get(&id));
            let name = node
                .name
                .as_deref()
                .map(|v| normalize_text(v, NAME_MAX))
                .filter(|v: &String| !v.is_empty());
            let role = node.role.clone().filter(|r| !r.is_empty());
            let text = node
                .description
                .as_deref()
                .or(node.value.as_deref())
                .map(|v| normalize_text(v, DOM_TEXT_MAX))
                .filter(|v: &String| !v.is_empty());
            BrowserSnapshotDomNode {
                tag: geo.map(|g| g.tag.clone()).unwrap_or_default(),
                depth: depths.get(node.node_id.as_str()).copied().unwrap_or(0) as i64,
                in_viewport: geo.map(|g| g.in_viewport).unwrap_or(false),
                r#ref: ref_by_node.get(node.node_id.as_str()).cloned(),
                role,
                name,
                text,
                attributes: None,
            }
        })
        .collect();

    BrowserSnapshot {
        url: url.to_string(),
        title: title.to_string(),
        dom: Some(dom),
        dom_truncated: Some(dom_truncated),
        elements,
        truncated: actionable.len() > max_elements,
    }
}

fn build_element(
    node: &RawAxNode,
    rf: &str,
    ref_by_node: &HashMap<&str, String>,
    parent_of: &HashMap<&str, &str>,
    geometry: &Geometry,
) -> BrowserSnapshotElement {
    let geo = node.backend_dom_node_id.and_then(|id| geometry.get(&id));
    let name = node
        .name
        .as_deref()
        .map(|v| normalize_text(v, NAME_MAX))
        .filter(|v: &String| !v.is_empty());
    let text = node
        .description
        .as_deref()
        .map(|v| normalize_text(v, ELEMENT_TEXT_MAX))
        .filter(|v: &String| !v.is_empty());
    let value = node
        .value
        .as_deref()
        .map(|v| normalize_text(v, VALUE_MAX))
        .filter(|v: &String| !v.is_empty());

    // parentRef: Find the nearest allocated ref ancestor (snapshot.ts:133-138).
    let mut parent_ref: Option<String> = None;
    let mut cursor: Option<&str> = Some(node.node_id.as_str());
    let mut hops = 0usize;
    while let Some(id) = cursor {
        if hops > 64 {
            break; // Ring/ultra-deep protection
        }
        if id != node.node_id.as_str() {
            if let Some(found) = ref_by_node.get(id) {
                parent_ref = Some(found.clone());
                break;
            }
        }
        cursor = parent_of.get(id).copied();
        hops += 1;
    }
    let _ = rf;

    BrowserSnapshotElement {
        r#ref: rf.to_string(),
        tag: geo.map(|g| g.tag.clone()).unwrap_or_default(),
        role: node.role.clone().filter(|r| !r.is_empty()),
        name,
        text,
        value,
        disabled: if node.disabled { Some(true) } else { None },
        checked: node.checked.filter(|_| {
            matches!(
                node.role.as_deref(),
                Some("checkbox") | Some("radio") | Some("switch")
            )
        }),
        selector: geo.map(|g| g.selector.clone()).unwrap_or_default(),
        xpath: geo.map(|g| g.xpath.clone()).unwrap_or_default(),
        rect: geo.map(|g| g.rect).unwrap_or_default(),
        in_viewport: geo.map(|g| g.in_viewport).unwrap_or(false),
        parent_ref,
        frame_path: None,
        attributes: None,
    }
}

fn build_parent_map<'a>(ordered: &[&'a RawAxNode]) -> HashMap<&'a str, &'a str> {
    let mut parent: HashMap<&str, &str> = HashMap::new();
    for node in ordered {
        for child in &node.child_ids {
            parent.insert(child.as_str(), node.node_id.as_str());
        }
    }
    parent
}

fn compute_depths<'a>(ordered: &[&'a RawAxNode]) -> HashMap<&'a str, usize> {
    let parent = build_parent_map(ordered);
    let mut depths: HashMap<&str, usize> = HashMap::new();
    for node in ordered {
        let mut d = 0usize;
        let mut cursor = Some(node.node_id.as_str());
        let mut guard = 0usize;
        while let Some(id) = cursor {
            if guard > 512 {
                break;
            }
            match parent.get(id) {
                Some(p) => {
                    cursor = Some(*p);
                    d += 1;
                }
                None => {
                    cursor = None;
                }
            }
            guard += 1;
        }
        depths.insert(node.node_id.as_str(), d.saturating_sub(1));
    }
    depths
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, role: &str, name: &str) -> RawAxNode {
        RawAxNode {
            node_id: id.to_string(),
            ignored: false,
            role: Some(role.to_string()),
            name: Some(name.to_string()),
            value: None,
            description: None,
            focusable: false,
            editable: false,
            disabled: false,
            checked: None,
            child_ids: Vec::new(),
            backend_dom_node_id: None,
        }
    }

    #[test]
    fn normalize_text_mirrors_ts_trim_collapse_slice() {
        // snapshot.ts:36-40: trim + \s+→' ' + slice(max)
        assert_eq!(normalize_text(" a b\n c ", 120), "a b c");
        assert_eq!(normalize_text("hello", 3), "hel");
        // unicode safe: truncate by char
        assert_eq!(normalize_text("héllo", 3), "hél");
    }

    #[test]
    fn refs_assigned_in_dom_order_like_ts() {
        //Document sequence root > a > b > c, ref in sequence e1, e2, e3 (TS snapshot.ts:122 index+1).
        let mut root = node("1", "RootWebArea", "page");
        root.child_ids = vec!["2".into(), "3".into(), "4".into()];
        let a = node("2", "button", "Alpha");
        let b = node("3", "link", "Beta");
        let c = node("4", "textbox", "Gamma");
        // Array variables are placed first and then references are taken: directly passing &[...] a temporary array will make the returned reference dangling.
        let pool = [root.clone(), a, b, c, root.clone()];
        let ordered = ax_nodes_in_dom_order(&pool, None);
        let snapshot = build_snapshot(
            &ordered,
            "http://x/",
            "T",
            &Geometry::new(),
            &SnapshotOptions::default(),
        );
        let refs: Vec<&str> = snapshot.elements.iter().map(|e| e.r#ref.as_str()).collect();
        assert_eq!(refs, vec!["e1", "e2", "e3"]);
        assert_eq!(snapshot.elements[0].role.as_deref(), Some("button"));
        assert_eq!(snapshot.elements[0].name.as_deref(), Some("Alpha"));
        assert!(!snapshot.truncated);
    }

    #[test]
    fn ignored_nodes_excluded_and_truncated_flags_correct() {
        let mut root = node("1", "RootWebArea", "");
        let hidden = RawAxNode {
            ignored: true,
            ..node("2", "button", "hidden one")
        };
        let visible = node("3", "button", "visible one");
        root.child_ids = vec!["2".into(), "3".into()];
        let pool = [root.clone(), hidden, visible];
        let ordered = ax_nodes_in_dom_order(&pool, None);
        let snapshot = build_snapshot(
            &ordered,
            "u",
            "t",
            &Geometry::new(),
            &SnapshotOptions::default(),
        );
        assert_eq!(
            snapshot.elements.len(),
            1,
            "ignored nodes are not included in elements (TS hidden filter snapshot.ts:119-121)"
        );
        assert_eq!(snapshot.elements[0].name.as_deref(), Some("visible one"));

        //Include when includeHidden=true
        let snapshot = build_snapshot(
            &ordered,
            "u",
            "t",
            &Geometry::new(),
            &SnapshotOptions {
                include_hidden: true,
                ..Default::default()
            },
        );
        assert_eq!(snapshot.elements.len(), 2);

        // truncated: exceeds maxElements
        let many: Vec<RawAxNode> = (0..5)
            .map(|i| node(&i.to_string(), "button", "b"))
            .collect();
        let mut root2 = node("root", "RootWebArea", "");
        root2.child_ids = (0..5).map(|i| i.to_string()).collect();
        let mut pool = vec![root2.clone()];
        pool.extend(many);
        let ordered = ax_nodes_in_dom_order(&pool, Some("root"));
        let snapshot = build_snapshot(
            &ordered,
            "u",
            "t",
            &Geometry::new(),
            &SnapshotOptions {
                max_elements: 3,
                ..Default::default()
            },
        );
        assert_eq!(snapshot.elements.len(), 3);
        assert!(
            snapshot.truncated,
            "actionable(5) > max(3) → truncated(TS snapshot.ts:205)"
        );
    }

    #[test]
    fn checkbox_maps_checked_and_radio_only() {
        let mut check = node("2", "checkbox", "Accept");
        check.checked = Some(true);
        let mut notcb = node("3", "button", "Click");
        notcb.checked = Some(true); // Non-checkbox/radio/switch checked should be discarded
        let root = {
            let mut r = node("1", "RootWebArea", "");
            r.child_ids = vec!["2".into(), "3".into()];
            r
        };
        let pool = [root, check, notcb];
        let ordered = ax_nodes_in_dom_order(&pool, None);
        let snapshot = build_snapshot(
            &ordered,
            "u",
            "t",
            &Geometry::new(),
            &SnapshotOptions::default(),
        );
        assert_eq!(snapshot.elements[0].checked, Some(true));
        assert_eq!(snapshot.elements[1].checked, None);
    }

    #[test]
    fn geometry_injects_tag_rect_selector() {
        let mut geo = Geometry::new();
        geo.insert(
            100,
            ResolvedGeometry {
                tag: "button".into(),
                rect: BrowserElementRect {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 30,
                },
                selector: "body > button:nth-of-type(1)".into(),
                xpath: "/html/body/button[1]".into(),
                in_viewport: true,
            },
        );
        let mut btn = node("1", "button", "OK");
        btn.backend_dom_node_id = Some(100);
        let pool = [btn];
        let ordered = ax_nodes_in_dom_order(&pool, None);
        let snapshot = build_snapshot(&ordered, "u", "t", &geo, &SnapshotOptions::default());
        let el = &snapshot.elements[0];
        assert_eq!(el.tag, "button");
        assert_eq!(el.rect.x, 10);
        assert!(el.in_viewport);
        assert_eq!(el.selector, "body > button:nth-of-type(1)");
    }

    #[test]
    fn parses_real_ax_tree_fixture_shape() {
        // Structure mirrors the real response of CDP Accessibility.getFullAXTree (including properties array).
        let result = json!({
            "nodes": [
                {
                    "nodeId": "1",
                    "ignored": false,
                    "role": {"value": "WebArea"},
                    "name": {"value": "My Page"},
                    "childIds": ["2"],
                    "backendDOMNodeId": 3
                },
                {
                    "nodeId": "2",
                    "ignored": false,
                    "role": {"value": "button"},
                    "name": {"value": "Submit"},
                    "childIds": [],
                    "properties": [
                        {"name": "focusable", "value": {"type": "boolean", "value": true}},
                        {"name": "pressed", "value": {"type": "boolean", "value": false}}
                    ],
                    "backendDOMNodeId": 4
                }
            ]
        });
        let nodes = parse_ax_tree(&result);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].role.as_deref(), Some("button"));
        assert!(nodes[1].focusable, "Resolve focusable from properties[]");
        assert_eq!(nodes[1].backend_dom_node_id, Some(4));
        assert_eq!(nodes[0].name.as_deref(), Some("My Page"));
    }

    #[test]
    fn snapshot_serializes_in_ts_field_order() {
        // dom must precede elements (truncated readability constraints of TS snapshot.ts:64-69 comments).
        let pool = [node("1", "button", "B")];
        let ordered = ax_nodes_in_dom_order(&pool, None);
        let snapshot = build_snapshot(
            &ordered,
            "http://u/",
            "Title",
            &Geometry::new(),
            &SnapshotOptions::default(),
        );
        let value = serde_json::to_value(&snapshot).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        let dom_pos = keys.iter().position(|k| *k == "dom").unwrap();
        let el_pos = keys.iter().position(|k| *k == "elements").unwrap();
        assert!(dom_pos < el_pos, "dom must precede elements: {keys:?}");
        assert_eq!(value["url"], "http://u/");
    }

    #[test]
    fn parent_ref_links_nested_actionable() {
        // The outer link wraps the inner button: the inner parentRef points to the outer ref (TS snapshot.ts:133-138).
        let mut outer = node("1", "link", "Card");
        outer.child_ids = vec!["2".into()];
        let inner = node("2", "button", "Buy");
        let pool = [outer.clone(), inner];
        let ordered = ax_nodes_in_dom_order(&pool, Some("1"));
        // outer ranks before inner → outer=e1, inner=e2, inner.parentRef=e1
        assert_eq!(ordered[0].node_id, "1");
        let snapshot = build_snapshot(
            &ordered,
            "u",
            "t",
            &Geometry::new(),
            &SnapshotOptions::default(),
        );
        assert_eq!(snapshot.elements[1].parent_ref.as_deref(), Some("e1"));
    }
}

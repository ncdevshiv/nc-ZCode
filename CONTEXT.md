# ZCode Plugin Store (Plugin Store)

A domain glossary for the plugin settings page and its marketplace browsing/installation experience. This document uniformly defines store-related terms for use on pages, services, and documents.

## Language

### Market and Source

**Official Marketplace**:
The only distribution channel officially operated by ZCode, the market ID is `zcode-plugins-official`, content = built-in plug-in + CDN plug-in. It is a "distribution channel" rather than "author attribution" - which can include plug-ins from community authors.
_Avoid_: "Official" refers to all trusted markets

**Builtin Plugin**:
A plug-in that is distributed with the application package and seeded into the official market at startup. Is a subset of the official plug-ins.
_Avoid_: pre-installed plug-in, bundled plug-in (available in spoken language, documents are unified "built-in")

**CDN Plugin（CDN Plugin）**:
In the official market, plug-ins are distributed in sha256-verified zip packages through the official CDN and can be downloaded and installed on demand.
_Avoid_: Network plug-in, online plug-in

**Personal Source**:
Sources of all plug-ins added by users: git/GitHub/URL/local directory market, inline plug-ins.
_Avoid_: None

**Catalog Auto-Refresh (catalog automatic refresh)**:
When entering the store page, the throttling background refresh of the Official Marketplace directory is invisible to the user; only the official market is covered.
_Avoid_: Mixed with Manual Refresh; call it "check for updates" (updating the subscript is just a by-product of refreshing)

**Manual Refresh**:
The market-wide refresh triggered by the refresh button on the top bar of the store page is not affected by automatic refresh throttling.
_Avoid_: Refresh, check for updates (spoken language available, documents unified "manual refresh")

### Store page structure

**Public Segment**:
One of the sections of the store list page, displaying and only displaying the catalog of the official market (Featured + Category section).
_Avoid_: official tab, store tab

**Personal Segment**:
Another section of the store listing page displays a catalog of all personal sources, grouped by market.
_Avoid_: Third-party tab, my tab

**Featured**:
A curated area at the top of the public segment, the list is remotely controlled by the `featured` field of the official CDN directory. Exists only in public segments.
_Avoid_: Mixed with Recommended

**Installed Strip**:
There is a row of installed plug-in icons at the top of the list page. Click the icon to enter the details page.
_Avoid_: Installed list (that's for the Manage Installed view)

**Manage Installed View**:
The management interface entered by the gear on the right side of the installed bar carries plug-in-level start-stop switches, updates, uninstalls, and enablement status filters.
_Avoid_: Installed tab (old IA term, deprecated)

### Metadata

**Store Listing**:
Display metadata carried by directory entries: display name, icon, category, developer, website/privacy policy/terms of service link, hero picture, example prompt words. Describes "how to present it in the store" and does not affect the functionality of the plugin.
_Avoid_: Plugin metadata (vague, may refer to manifest)

**Plugin Manifest**:
Functional definition of `plugin.json` in the plugin package (commands/agents/skills/hooks/mcpServers/userConfig…). Describe "what the plug-in is and does".
_Avoid_: marketplace.json (that’s a directory, not a list)

**Example Prompt**:
Clickable prompt words provided by Store Listing, click to create a new session and pre-fill it (not automatically sent). It is the only "New Session" entrance on the details page.
_Avoid_: shortcut commands, prompt templates, try now

### Life cycle status

**Plugin Lifecycle**:
The user starts from discovering the plug-in, and goes through viewing, installation, configuration, start, stop, use, check for updates, upgrade, persistence recovery, until uninstalling or restoring the complete product path of the built-in plug-in. Each phase must simultaneously verify visible UI state and corresponding persistence or runtime results.
_Avoid_: Only "successful installation" is called the complete life cycle

**Restorable Builtin**:
Builtin Plugin that was uninstalled by the user and entered a persistence-suppressed state. App restarts must not automatically reseed; it continues to appear in the Public Segment and perform a clean recovery through the Installation portal.
_Avoid_: CDN plug-in not installed, built-in plug-in temporarily disabled

**Orphaned Installed Plugin**:
Corresponds to plug-ins whose Personal Source has been deleted, but the installation directory and user data remain. It can still be used, configured, started, stopped, and uninstalled; it cannot be updated until the source is re-added, and the directory association is restored after the same source is re-added.
_Avoid_: Damaged installation, missing manifest, uninstalled plugin

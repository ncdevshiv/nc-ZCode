# -*- coding: utf-8 -*-
# clippy ending: 1 deny error + 22 warnings.
# Principle: Real dead code is deleted by anchor point; related to the recorded boundary (account line of claim-boundary,
# attachmentStat) is also deleted instead of allow; if the semantics are really deviant (future without await), just add await.
import io
import re

failures = 0


def patch(path, pairs):
    global failures
    text = io.open(path, encoding="utf-8").read()
    for old, new, label in pairs:
        hits = text.count(old)
        if hits != 1:
            print("MISS(%d) [%s] in %s" % (hits, label, path))
            failures += 1
            continue
        text = text.replace(old, new)
        print("ok [%s]" % label)
    io.open(path, "w", encoding="utf-8", newline="").write(text)


def delete_block(path, start_anchor, end_kind, label, before_anchor=None):
    """Delete the entire block starting from start_anchor and ending with the item (`}` at the top of the column / at the end of `;`),
    And delete the doc comment immediately above it (if before_anchor is specified, it will be the first of the block). """
    global failures
    text = io.open(path, encoding="utf-8").read()
    start = text.find(start_anchor)
    if start == -1:
        print("MISS start [%s] in %s" % (label, path))
        failures += 1
        return
    head=start
    if before_anchor:
        doc = text.rfind(before_anchor, 0, start)
        if doc != -1:
            head=doc
    # End position of item: `}` or semicolon at the top of the column.
    if end_kind == "brace":
        match = re.compile(r"\n\}(\n)").search(text, start)
        end = match.start() + 1 + 1 if match else -1
    else:
        match = re.compile(r";\n").search(text, start)
        end = match.end() if match else -1
    if end == -1:
        print("MISS end [%s] in %s" % (label, path))
        failures += 1
        return
    # Eat the following blank line incidentally to avoid leaving double blank lines.
    while end < len(text) and text[end] == "\n":
        end += 1
    io.open(path, "w", encoding="utf-8", newline="").write(text[:head] + text[end:])
    print("ok [delete %s]" % label)base = "native/crates/"
git = base + "zcode-services-git/src/"
ext = base + "zcode-services-extensions/src/"
host = base + "zcode-host/src/"

# ── 1. Deny error: `<= 0` on usize has only one case == 0.
patch(
    ext + "model_selection.rs",
    [(" if separator <= 0 {", " if separator == 0 {", "separator == 0")],
)

# ── 2. task.rs: `let _ = <future>` is never executed (clippy::let_underscore_future).
# The epoch bump of bumpRuntimeEpoch has never taken effect, so add `.await`.
patch(
    base + "zcode-services-agent/src/task.rs",
    [
        (
            """ let _ = self.index.mutate(|index| {
                            if let Some(row) = index.tasks.get_mut(task_id) {
                                row.runtime_epoch = Some(row.runtime_epoch.unwrap_or(0) + 1);
                            }
                            Ok(())
                        });""",
            """ // It turns out to be `let _ = self.index.mutate(...)` - the unawaited future is
                        //Discard directly, epoch promotion is never actually executed; add await(clippy::let_underscore_future).
                        let _ = self
                            .index
                            .mutate(|index| {
                                if let Some(row) = index.tasks.get_mut(task_id) {
                                    row.runtime_epoch = Some(row.runtime_epoch.unwrap_or(0) + 1);
                                }
                                Ok(())
                            })
                            .await;""",
            "await the mutate future",
        ),
    ],
)

# ── 3. provider: pub(crate) function cannot return private types.
patch(
    base + "zcode-services-provider/src/catalog.rs",
    [
        (
            "/// Actual measurement results of a single detection.\nstruct MeasuredRun {","/// The actual measurement result of a single probe (`run_probe` is pub(crate), the return type must be visible to the same level).\npub(crate) struct MeasuredRun {",
            "MeasuredRun pub(crate)",
        ),
    ],
)

# ── 4. git: new_ret_no_self, type alias, strip_prefix.
patch(
    git + "commit_message.rs",
    [
        (
            " pub fn new(store: Arc<ProviderStore>) -> Arc<dyn CommitMessageGenerator> {",
            " // clippy::new_ret_no_self: Return Arc<dyn Trait>, semantically named from_store.\n"
            " pub fn from_store(store: Arc<ProviderStore>) -> Arc<dyn CommitMessageGenerator> {",
            "rename generator ctor",
        ),
    ],
)
patch(
    base + "zcode-services-git/src/lib.rs",
    [
        (
            "ProviderCommitMessageGenerator::new(store)",
            "ProviderCommitMessageGenerator::from_store(store)",
            "update ctor caller",
        ),
    ],
)
patch(
    git + "parse.rs",
    [
        (
            """/// `git config --show-scope --show-origin --get <key>`.
pub fn parse_git_config_value(
    result: &GitCommandResult,
) -> Result<(Option<String>, Option<String>, Option<String>), ServiceError> {""",
            """/// Triplet of `git config --get`: (value, origin, scope).
pub type GitConfigGet = (Option<String>, Option<String>, Option<String>);

/// `git config --show-scope --show-origin --get <key>`.
pub fn parse_git_config_value(result: &GitCommandResult) -> Result<GitConfigGet, ServiceError> {""",
            "type alias for config triple",
        ),
    ],
)
patch(
    git + "path.rs",
    [
        (
            """ if path.starts_with('/') {
        return ("/".to_string(), path[1..].to_string());
    }""",
            """ if let Some(rest) = path.strip_prefix('/') {
        return ("/".to_string(), rest.to_string());
    }""",
            "strip_prefix",
        ),
    ],)

# ── 5. octal escape: `\0` followed by a number (such as `\\01`) reads like octal. Use \x00 to disambiguate,
# Bytes are exactly the same (clippy::octal_escapes ×9).
for path in [git + "parse.rs", git + "refs.rs"]:
    text = io.open(path, encoding="utf-8").read()
    updated = re.sub(r"\\0(?=[0-9])", r"\\x00", text)
    count = len(re.findall(r"\\0(?=[0-9])", text))
    if count:
        io.open(path, "w", encoding="utf-8", newline="").write(updated)
    print("ok [octal escapes x%d] %s" % (count, path.split("/")[-1]))

# ── 6. git Really dead code (never referenced by any call site/test).
delete_block(git + "repo/mod.rs", "pub(crate) fn required_string_array", "brace", "required_string_array")
delete_block(git + "repo/mod.rs", "pub(crate) fn optional_string_array", "brace", "optional_string_array")
#Two constants + their comments are deleted together (single block deletion will miss the second const): processed by line, no regular expression is written.
lines = io.open(git + "repo/mod.rs", encoding="utf-8").read().split("\n")
start = next(
    (i for i, line in enumerate(lines) if line.startswith("/// Re-exported so")),
    None,
)
if start is None:
    print("MISS COMMAND_* doc comment")
    failures += 1
else:
    end = start
    while end < len(lines) and not lines[end].startswith("pub(crate) const COMMAND_PUSH_TIMEOUT"):
        end += 1
    if end >= len(lines):
        print("MISS COMMAND_PUSH_TIMEOUT line")
        failures += 1
    else:
        del lines[start : end + 1]
        io.open(git + "repo/mod.rs", "w", encoding="utf-8", newline="").write("\n".join(lines))
        print("ok [delete COMMAND_* re-exports]")

# ── 7. host attachment: stat/AttachmentStat/Preview upper limit service is
# claim-boundary removed `conversationAttachmentStatV4` (see §4.5) → remove instead of allow.
delete_block(host + "attachment.rs", "pub fn stat(&self, session_id: &str, ref_value: &str)", "brace", "AttachmentStore::stat")delete_block(host + "attachment.rs", "pub struct AttachmentStat", "brace", "AttachmentStat", before_anchor="pub struct AttachmentStat")
# Unused constants (along with clinging comments).
text = io.open(host + "attachment.rs", encoding="utf-8").read()
match = re.search(r"(//[^\n]*\n)*const ATTACHMENT_PREVIEW_MAX_BYTES[^;]+;\n", text)
if match:
    text = text[: match.start()] + text[match.end() :]
    io.open(host + "attachment.rs", "w", encoding="utf-8", newline="").write(text)
    print("ok [delete ATTACHMENT_PREVIEW_MAX_BYTES]")
else:
    print("MISS ATTACHMENT_PREVIEW_MAX_BYTES")
    failures += 1

# ── 8. Extended family dead code: account line list (not projected under claim boundary, never used),
# cli_config_dir replaced by AppPaths.
delete_block(ext + "model_selection.rs", "/// `BUILTIN_MODEL_PROVIDER_IDS`.", "semi", "ACCOUNT_IDS", before_anchor="/// `BUILTIN_MODEL_PROVIDER_IDS`.")
delete_block(ext + "user_config.rs", "fn cli_config_dir", "brace", "cli_config_dir")

# ── 9. sessions_index: 10 The parameter is a one-to-one mapping of the TS function of the same name. Changing it to a parameter object will expand the transplantation area.
patch(
    host + "sessions_index.rs",
    [],
)

# ── 10. client e2e unused import.
client_test = base + "zcode-services-client/tests/client_channels_e2e.rs"
text = io.open(client_test, encoding="utf-8").read()
for line in text.split("\n"):
    if "Channel" in line and line.strip().startswith("use") and "zcode_channels::" in line:
        print("client import line:", line)

if failures:
    raise SystemExit("FAILED: %d" % failures)
print("phase 1 clean")
"""Rename scanner: measure the zcode -> nc-code surface before touching it.

Scan mode (default) reports what each rename class would change and how often, and writes nothing.
Apply mode is deliberately NOT implemented here: the sweep has to run while no other process is
editing the tree, and it has to be followed by per-crate cargo gates, so it stays a reviewed,
single-shot operation rather than a flag on a helper script.

Classes, from specs/nc-code-rename-plan.md:
  A  internal identifiers — crate package names, Rust crate paths, binary names, gate scripts.
  C  environment variables — renamed only with a dual-name read, because users and the desktop
     shell set them today.
  B  on-disk state — `.zcode/`, provider store paths, plugin dirs. Never renamed silently: that is
     a migration with a decision, not a find-and-replace.
"""

import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
CRATES = os.path.join(ROOT, "crates")

# Class A patterns, applied to Rust/TOML/JSON/MJS/text under native/.
CLASS_A = [
    ("crate path in Rust (`zcode_foo::`)", re.compile(r"\bzcode_([a-z0-9_]+)(?:::|\b)")),
    ("cargo package name (`zcode-foo`)", re.compile(r"\bzcode-([a-z0-9-]+)")),
]
# Class C / B markers: reported, never rewritten by the sweep.
CLASS_C = re.compile(r"\bZCODE_[A-Z0-9_]+\b")
CLASS_B = re.compile(r"\.zcode[a-z-]*\b|\bzcodeignore\b")
# Things that look like class A but are not ours to rename in this sweep.
EXCLUDED = re.compile(r"@zcode/|apps/zcode-cli|packages/[a-z-]+/|zcode-protocol")

TEXT_EXTENSIONS = (".rs", ".toml", ".mjs", ".json", ".md", ".ts")


def walk():
    for directory, dirs, names in os.walk(ROOT):
        dirs[:] = [d for d in dirs if d not in ("target", ".git", "node_modules")]
        for name in names:
            if name.endswith(TEXT_EXTENSIONS):
                yield os.path.join(directory, name)


def main():
    class_a = {}
    class_c = set()
    class_b = set()
    files_with_a = 0
    for path in walk():
        try:
            text = io.open(path, encoding="utf-8").read()
        except (UnicodeDecodeError, OSError):
            continue
        hits = []
        for label, pattern in CLASS_A:
            for match in pattern.finditer(text):
                token = match.group(0)
                if EXCLUDED.search(token):
                    continue
                hits.append((label, token))
        if hits:
            files_with_a += 1
        for label, token in hits:
            class_a[token] = class_a.get(token, 0) + 1
        class_c.update(CLASS_C.findall(text))
        class_b.update(CLASS_B.findall(text))
    print("files under native/ carrying a class-A token: %d" % files_with_a)
    print("distinct class-A tokens: %d, total occurrences: %d" % (len(class_a), sum(class_a.values())))
    for token, count in sorted(class_a.items(), key=lambda row: -row[1])[:25]:
        print("   %5d  %s" % (count, token))
    print("class-C env vars (dual-name work, %d): %s" % (len(class_c), " ".join(sorted(class_c))))
    print("class-B on-disk names (migration, %d): %s" % (len(class_b), " ".join(sorted(class_b))))
    crate_dirs = sorted(name for name in os.listdir(CRATES) if os.path.isdir(os.path.join(CRATES, name)))
    print("crate directories to rename: %d" % len([d for d in crate_dirs if d.startswith("zcode-")]))


if __name__ == "__main__":
    sys.exit(main())

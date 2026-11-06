"""Comment-only translation runner.

Reads a JSON manifest of edits and applies them bottom-up (descending line number) so that
earlier line numbers stay valid even when a replacement spans more lines than the original.

Manifest shape:
    { "path": [ { "line": N, "needle": "chinese substring on that line", "new": "replacement text\n(may be several lines)" } ] }

Every edit asserts that the given line exists and that the needle occurs on it; the stated line
number is the locator, so a needle that also occurs elsewhere only produces a warning. With
`--check` nothing is written and every mismatch is reported at once.
"""

import io
import json
import sys


def main(manifest_path, check_only=False):
    manifest = json.load(io.open(manifest_path, encoding="utf-8"))
    problems = []
    for path, edits in manifest.items():
        lines = io.open(path, encoding="utf-8").read().split("\n")
        for edit in sorted(edits, key=lambda entry: entry["line"], reverse=True):
            index = edit["line"] - 1
            needle = edit["needle"]
            in_range = 0 <= index < len(lines)
            if not in_range or needle not in lines[index]:
                problems.append("%s:%d needle %r not on that line; line is %r" % (
                    path,
                    edit["line"],
                    needle,
                    lines[index] if in_range else "<out of range>",
                ))
                if check_only:
                    continue
                raise AssertionError(problems[-1])
            hits = [number for number, line in enumerate(lines, start=1) if needle in line]
            if hits != [edit["line"]]:
                print("warn: needle %r also on lines %s (applying at %d)" % (needle, hits, edit["line"]))
            if not check_only:
                lines[index:index + 1] = edit["new"].split("\n")
        if not check_only:
            io.open(path, "w", encoding="utf-8", newline="\n").write("\n".join(lines))
            print("applied %d edits to %s" % (len(edits), path))
    for problem in problems:
        print("MISMATCH " + problem)
    if problems:
        print("%d mismatches" % len(problems))


if __name__ == "__main__":
    main(sys.argv[1], check_only="--check" in sys.argv[2:])

"""Turn compiler, clippy, rustfmt and test output into check-run annotations.

Used by the temporary compile-report workflow: this environment can read
annotations but not job logs, so diagnostics are republished as annotations.
Annotation messages are capped, so long outputs are chunked.
"""

import os
import re
import sys

MAX_MESSAGE = 900


def emit(kind: str, path: str, line: str, message: str) -> None:
    message = message.replace("\n", " ⏎ ").replace("%", "%25").replace("\r", "")
    sys.stdout.write(f"::{kind} file={path},line={line},col=1::{message}\n")


def read(path: str) -> list[str]:
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            return handle.read().splitlines()
    except OSError:
        return []


def chunk(text: str) -> list[str]:
    if len(text) <= MAX_MESSAGE:
        return [text]
    parts = []
    while text:
        cut = text.rfind(" ", 0, MAX_MESSAGE)
        if cut <= 0:
            cut = MAX_MESSAGE
        parts.append(text[:cut])
        text = text[cut:].lstrip()
    return parts


def annotate_build(path: str) -> None:
    lines = read(path)
    errors = 0
    for line in lines:
        if ": error" not in line and not line.startswith("error"):
            continue
        errors += 1
        parts = line.split(":", 3)
        if len(parts) >= 4 and parts[0].endswith(".rs"):
            # `--message-format=short` puts the detail on the next indented
            # line, so include it: "mismatched types" alone is unusable.
            detail = ""
            idx = lines.index(line)
            if idx + 1 < len(lines):
                nxt = lines[idx + 1].strip()
                if nxt and not nxt.startswith("error"):
                    detail = f" [{nxt[:300]}]"
            emit("error", parts[0], parts[1], f"{parts[2].strip()}: {parts[3]}{detail}".strip())
        else:
            emit("error", ".github", "1", line)
    if errors == 0:
        emit("notice", ".github", "1", f"{os.path.basename(path)}: no errors")


def annotate_fmt(path: str) -> None:
    """Republish the rustfmt diff, one annotation per hunk.

    rustfmt prints either "Diff in <path> at line <n>:" or, since 1.6x,
    "Diff in <path>:<n>:".
    """
    lines = read(path)
    if not lines:
        emit("notice", ".github", "1", "fmt: clean")
        return

    header = re.compile(r"^Diff in (.+?)(?: at line |:)(\d+):?$")
    current_file = None
    current_line = "1"
    hunk: list[str] = []
    emitted = 0

    def flush() -> None:
        nonlocal emitted
        if current_file and hunk:
            text = " ".join(part for part in hunk if part).strip()
            for part in chunk(f"fmt: {text}"):
                emit("error", current_file, current_line, part)
                emitted += 1

    for line in lines:
        match = header.match(line)
        if match:
            flush()
            current_file = match.group(1)
            current_line = match.group(2)
            hunk = []
            continue
        if line.startswith(("+++", "---", "@@")):
            continue
        if current_file:
            hunk.append(line.rstrip())
    flush()
    if emitted == 0:
        for part in chunk(" ".join(lines))[:6]:
            emit("notice", ".github", "1", f"fmt raw: {part}")


def annotate_tests(path: str) -> None:
    """Report failing test names, then the captured output of each failure."""
    lines = read(path)
    failed: list[str] = []
    seen: set[str] = set()
    for line in lines:
        if line.startswith("test ") and line.endswith("FAILED"):
            failed.append(line)
            if line not in seen:
                seen.add(line)
                emit("error", ".github", "1", line)
    if not failed:
        emit("notice", ".github", "1", "tests: no reported failures in this log")
        return

    # Each failure block starts with "---- <name> stdout ----".
    block: list[str] = []
    in_block = False
    for line in lines:
        if line.startswith("---- ") and line.endswith(" stdout ----"):
            if block:
                for part in chunk(" ".join(block).strip()):
                    emit("error", ".github", "1", f"assert: {part}")
            block = []
            in_block = True
            continue
        if in_block:
            if line.startswith("failures:") or line.startswith("---- "):
                in_block = False
                if block:
                    for part in chunk(" ".join(block).strip()):
                        emit("error", ".github", "1", f"assert: {part}")
                block = []
            else:
                block.append(line.strip())
    if block:
        for part in chunk(" ".join(block).strip()):
            emit("error", ".github", "1", f"assert: {part}")


def main() -> None:
    for target in sys.argv[1:]:
        name = os.path.basename(target)
        if name.startswith("fmt"):
            annotate_fmt(target)
        elif name.startswith("test") or name.startswith("desktop") or name.startswith("daemon"):
            annotate_build(target)
            annotate_tests(target)
        else:
            annotate_build(target)


if __name__ == "__main__":
    main()

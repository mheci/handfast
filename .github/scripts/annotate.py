"""Turn compiler output into check-run annotations.

Used by the temporary compile-report workflow: this environment can read
annotations but not job logs, so diagnostics are republished as annotations.
"""

import sys


def emit(kind: str, path: str, line: str, message: str) -> None:
    sys.stdout.write(f"::{kind} file={path},line={line},col=1::{message}\n")


def main() -> None:
    build_path = sys.argv[1]
    try:
        with open(build_path, "r", encoding="utf-8", errors="replace") as handle:
            lines = handle.read().splitlines()
    except OSError as error:
        emit("error", ".github", "1", f"build output unreadable: {error}")
        return

    for line in lines:
        if ": error" in line or line.startswith("error"):
            parts = line.split(":", 3)
            if len(parts) >= 4 and parts[0].endswith(".rs"):
                emit("error", parts[0], parts[1], f"{parts[2]}: {parts[3]}"[:900])
            else:
                emit("error", ".github", "1", line[:900])
    errors = sum(1 for line in lines if ": error" in line or line.startswith("error"))
    if errors == 0:
        emit("notice", ".github", "1", "cargo build produced no errors")

    if len(sys.argv) > 2:
        try:
            with open(sys.argv[2], "r", encoding="utf-8", errors="replace") as handle:
                fmt = handle.read().splitlines()
        except OSError:
            fmt = []
        for line in fmt:
            if line.startswith("Diff in "):
                target = line[len("Diff in ") :].split(" at line")[0]
                emit("error", target, "1", "rustfmt wants a change here")


if __name__ == "__main__":
    main()

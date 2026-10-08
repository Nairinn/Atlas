#!/usr/bin/env python3
"""Fail if an SDK does not cover the whole gateway surface.

An SDK that silently lags the API looks finished, so nobody notices the
missing endpoint until a user needs it and reaches for the raw HTTP client
instead. And nothing in a gateway diff reminds you that three SDKs in
another directory describe the same surface.

So every (method, path) the router serves must appear in each SDK's source
as a whole quoted path literal in a file that also names that method —
not as a bare substring anywhere, which let a path count as covered while
its method was missing. Route files this script does not know how to
mount fail loudly, the same way check-openapi-routes.py handles them.
"""
import re
import sys
from pathlib import Path

ROUTES_DIR = Path("services/gateway/src/routes")
PREFIXES = {
    "auth.rs": "/v1/auth",
    "geo.rs": "/v1/geo",
    "payments.rs": "/v1/payments",
    # Health probes are not part of the customer-facing API, so no SDK is
    # expected to expose them.
    "ops.rs": None,
}

SDKS = {
    "typescript": Path("sdks/typescript/src"),
    "rust": Path("sdks/rust/src"),
    "dart": Path("sdks/dart/lib"),
}

# `.route("/path", get(handler))`.
ROUTE_RE = re.compile(r'\.route\(\s*"([^"]+)"\s*,\s*(.+?)\)\s*$', re.MULTILINE)
METHOD_RE = re.compile(r"\b(get|post|put|patch|delete)\s*\(")


def gateway_surface():
    """{(method, path)} as the gateway actually serves it."""
    known = set(PREFIXES) | {"mod.rs"}
    actual = {p.name for p in ROUTES_DIR.glob("*.rs")}
    unmapped = actual - known
    if unmapped:
        raise SystemExit(
            f"{ROUTES_DIR} has files this script does not know how to mount: "
            f"{sorted(unmapped)}. Add them to PREFIXES."
        )

    surface = set()
    for file, prefix in PREFIXES.items():
        if prefix is None:
            continue
        source = ROUTES_DIR / file
        if not source.exists():
            continue
        for path, handlers in ROUTE_RE.findall(source.read_text()):
            for method in METHOD_RE.findall(handlers):
                # axum's `:id` is the SDKs' `{id}`/interpolated id; compare
                # on the stable prefix before the first parameter.
                stable = f"{prefix}{path}".split("/:")[0]
                surface.add((method.upper(), stable))
    return surface


def strip_comments(text):
    """Remove line comments so a path mentioned only in prose does not
    count as covered."""
    return re.sub(r"//[^\n]*", "", text)


def covers(surface, directory):
    """(method, path) pairs the SDK plausibly serves."""
    files = [
        (p, strip_comments(p.read_text()))
        for p in sorted(directory.rglob("*"))
        if p.is_file() and p.suffix in {".ts", ".rs", ".dart"}
    ]
    found = set()
    for method, path in surface:
        # SDKs name the method either as a function (`Request::get`,
        # `routing::get`) or as a string/value (`method: 'POST'`); either
        # spelling satisfies the method half of the pair. The path is
        # quoted in any of the three languages' styles.
        lower = rf"\b{method.lower()}\b"
        upper = rf"\b{method}\b"
        path_quoted = re.compile(rf"[\"'`]({re.escape(path)})[\"'`]")
        for _, text in files:
            if path_quoted.search(text) and (re.search(lower, text) or re.search(upper, text)):
                found.add((method, path))
                break
    return found


def main():
    expected = gateway_surface()
    if not expected:
        print("no routes parsed - is this running from the repo root?")
        return 2

    failed = False
    for name, directory in SDKS.items():
        if not directory.exists():
            print(f"{name}: directory {directory} is missing")
            failed = True
            continue
        covered = covers(expected, directory)
        missing = sorted(expected - covered)
        if missing:
            failed = True
            print(f"{name} SDK does not cover:")
            for method, path in missing:
                print(f"  - {method} {path}")
        else:
            print(f"{name}: covers all {len(expected)} (method, path) pairs")

    if failed:
        print("\nEvery SDK must cover the whole gateway surface.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

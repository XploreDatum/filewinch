#!/usr/bin/env python3
"""Renders scripts/release-notes.md.tmpl into a GitHub release body (stdout).

    python3 -I scripts/release-notes.py --version 0.9.0 --repo XploreDatum/filewinch \
        --windows-signed false [--minisign-public-key RWQ...]

{{VERSION}}, {{REPO}} and {{MINISIGN_PUBLIC_KEY}} are filled in; {{CHANGELOG}} becomes the
version's section of CHANGELOG.md (which must exist). Blocks between {{#name}} and {{/name}} are
kept only when their condition holds: windows_signed, windows_unsigned, minisign.
"""
import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def changelog_section(version: str) -> str:
    text = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    # "## [0.9.0] - 2026-11-20" up to the next "## " heading.
    m = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## |\Z)", text, re.S | re.M)
    if not m or not m.group(1).strip():
        sys.exit(f"release-notes.py: CHANGELOG.md has no '## [{version}]' section; add one before releasing")
    # Drop link reference definitions ("[0.9.0]: https://...") that close the file.
    body = re.sub(r"^\[[^\]]+\]: \S+\s*$", "", m.group(1), flags=re.M).strip()
    return "## What's in this release\n\n" + body


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--version", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--windows-signed", required=True, choices=["true", "false"])
    ap.add_argument("--minisign-public-key", default="")
    ap.add_argument("--template", default=str(ROOT / "scripts" / "release-notes.md.tmpl"))
    a = ap.parse_args()

    if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?", a.version):
        sys.exit(f"release-notes.py: '{a.version}' isn't a version like 0.9.0")
    flags = {
        "windows_signed": a.windows_signed == "true",
        "windows_unsigned": a.windows_signed == "false",
        "minisign": bool(a.minisign_public_key),
    }
    text = pathlib.Path(a.template).read_text(encoding="utf-8")
    for name, keep in flags.items():
        pattern = re.compile(r"\{\{#" + name + r"\}\}\n?(.*?)\{\{/" + name + r"\}\}\n?", re.S)
        text = pattern.sub(lambda m: m.group(1) if keep else "", text)
    values = {
        "VERSION": a.version,
        "REPO": a.repo,
        "MINISIGN_PUBLIC_KEY": a.minisign_public_key,
        "CHANGELOG": changelog_section(a.version),
    }
    for key, value in values.items():
        text = text.replace("{{" + key + "}}", value)
    left = re.findall(r"\{\{[^}]*\}\}", text)
    if left:
        sys.exit(f"release-notes.py: unfilled placeholders: {sorted(set(left))}")
    sys.stdout.write(re.sub(r"\n{3,}", "\n\n", text))


if __name__ == "__main__":
    main()

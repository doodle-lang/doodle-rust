#!/usr/bin/env python3
"""Golden language corpus (M1.12): extract Doodle code examples from the language
spec and keep the committed corpus in sync with it.

The spec lives in the sibling `discussions` repo; the corpus (fixtures + manifest,
plus the insta AST snapshots in `crates/doodle-core/tests/lang_corpus.rs`) lives
here in `doodle-rust`, self-contained so ordinary `cargo test` / the conformance
runner exercise it WITHOUT a `discussions` checkout. This script is the only part
that reads the spec; run it in the workspace (where `discussions` is a sibling).

    scripts/lang-corpus-sync.py            # check: fail if the corpus drifts from the spec
    scripts/lang-corpus-sync.py --write    # regenerate the manifest + fixtures from the spec

A code fence in `language.md` is tagged (M1.12): ```doodle (a Doodle example),
```grammar (EBNF), or ```text (a token/keyword inventory). Only ```doodle blocks
become fixtures; each is `golden` (extracted verbatim) unless the manifest's
`overrides` marks it `wrapped` (a documented substitution making an elided example
runnable) or `excluded` (with a reason). `grammar`/`text` blocks are recorded for
drift detection but produce no fixture.

`--write` accepts the current spec as the new truth (like `cargo insta accept`);
plain `check` fails on any drift — a changed/added/removed/retagged block, or a
fixture whose source no longer matches — so a spec edit can't silently rot the
corpus. Fence pairing is asserted balanced: an odd fence count is a hard error
(the trap that hid an orphan fence during development).
"""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve()
DOODLE_RUST = SCRIPT.parent.parent
WORKSPACE = DOODLE_RUST.parent
SPEC = WORKSPACE / "discussions" / "spec" / "language.md"
MANIFEST = DOODLE_RUST / "conformance" / "lang-corpus.json"
LANG_DIR = DOODLE_RUST / "conformance" / "v0.1" / "lang"

# Curated dispositions for doodle blocks that are not plain golden. Matched by
# section + a marker substring (not document-order index, which shifts when the
# spec gains/loses an earlier block) so the override tracks its block across edits.
OVERRIDES = [
    {
        "match": {"section": "10.3", "contains": "…"},
        "disposition": "wrapped",
        "wrap": {"find": "…", "replace": "show(i)"},
        "reason": "§10.3's only example elides the block body with `…`; "
        "substitute a minimal body so single dispatch gets golden-AST coverage.",
    },
]


def override_for(block):
    """The curated override matching this block (by section + marker), or None.
    A block matching more than one override is a curation error (hard fail)."""
    hits = [
        o
        for o in OVERRIDES
        if o["match"]["section"] == block["section"]
        and o["match"]["contains"] in block["body"]
    ]
    if len(hits) > 1:
        die(f"block #{block['index']} matches {len(hits)} overrides — ambiguous")
    return hits[0] if hits else None


def die(msg):
    print(f"lang-corpus-sync: {msg}", file=sys.stderr)
    sys.exit(1)


def sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def section_number(header):
    """The leading dotted section number of a heading's text (`9.1 Declaration` ->
    `9.1`), or None (appendices/untitled) — only doodle blocks need one. `header`
    is the heading text with its leading `#`s already stripped."""
    m = re.match(r"([0-9]+(?:\.[0-9]+)*)\b", header)
    return m.group(1) if m else None


# A CommonMark backtick fence: up to 3 leading spaces, >=3 backticks, then the
# rest of the line (the info string on an opener; must be blank on a closer).
FENCE = re.compile(r"^ {0,3}(`{3,})(.*)$")


def extract_blocks(spec_text):
    """Every fenced block in document order: {index, section, tag, body}.

    One CommonMark-consistent scan does opening, closing, and balance-checking, so
    they cannot disagree (the earlier three-different-patterns split silently
    dropped indented fences and miscounted content lines that start with backticks):
    the info string is its first token; a closing fence is an unadorned run of at
    least the opening length; a block still open at EOF is a hard error (which is
    how the orphan fence at EOF is caught)."""
    blocks = []
    last_header = ""
    inblock = False
    open_len = 0
    for line in spec_text.split("\n"):
        m = FENCE.match(line)
        if not inblock:
            h = re.match(r"^#{1,6}\s+(.*)", line)
            if h:
                last_header = h.group(1).strip()
            if m:
                inblock = True
                open_len = len(m.group(1))
                info = m.group(2).strip()
                tag = info.split()[0] if info else ""
                start_header, buf = last_header, []
        elif m and len(m.group(1)) >= open_len and m.group(2).strip() == "":
            inblock = False
            blocks.append(
                {
                    "index": len(blocks),
                    "section": section_number(start_header),
                    "tag": tag,
                    "body": "\n".join(buf),
                }
            )
        else:
            buf.append(line)
    if inblock:
        die(f"unterminated code fence in {SPEC.name}: a ``` opened but never closed")
    return blocks


def wrapped_body(block, ov):
    body = block["body"]
    if ov and ov.get("disposition") == "wrapped":
        find, repl = ov["wrap"]["find"], ov["wrap"]["replace"]
        if find not in body:
            die(f"block #{block['index']}: wrap target {find!r} not in the spec block")
        body = body.replace(find, repl)
    return body


def fixture_relpath(block):
    sec = block["section"]
    if not sec:
        die(
            f"doodle block #{block['index']} is under a non-numbered heading "
            f"({block['section']!r}), so it has no `L<section>` clause: retag it, "
            f"move it under a numbered section, or add an OVERRIDE excluding it"
        )
    return f"L{sec}/spec-b{block['index']:02d}.doodle"


def fixture_text(block, ov):
    sec = block["section"]
    body = wrapped_body(block, ov)
    return f"#! clause: L{sec}\n#! mode: static\n#! stage: full\n{body}\n"


def plan_corpus(spec_text):
    """The corpus the current spec implies: the manifest `blocks` list + the map
    of fixture relpath -> intended file text."""
    blocks_out, fixtures = [], {}
    for b in extract_blocks(spec_text):
        ov = override_for(b)
        entry = {
            "index": b["index"],
            "section": b["section"],
            "tag": b["tag"],
            "sha256": sha(b["body"]),
        }
        if b["tag"] == "doodle":
            disp = ov["disposition"] if ov else "golden"
            entry["disposition"] = disp
            if disp == "excluded":
                entry["reason"] = ov["reason"]
            else:
                rel = fixture_relpath(b)
                entry["fixture"] = rel
                fixtures[rel] = fixture_text(b, ov)
                if disp == "wrapped":
                    entry["wrap"] = ov["wrap"]
                    entry["reason"] = ov["reason"]
        else:
            entry["disposition"] = f"excluded:{b['tag']}"
        blocks_out.append(entry)
    return blocks_out, fixtures


def load_manifest():
    if not MANIFEST.exists():
        die(f"no manifest at {MANIFEST} — run with --write first")
    return json.loads(MANIFEST.read_text())


def build_manifest(blocks):
    """The full manifest object the spec implies — regenerated identically by write
    and check, so check validates every field (not just `blocks`): a hand-edit that
    makes `counts`/`note`/`spec` lie is caught too."""
    counts = {}
    for b in blocks:
        counts[b["disposition"]] = counts.get(b["disposition"], 0) + 1
    return {
        "note": "Generated by scripts/lang-corpus-sync.py from "
        "discussions/spec/language.md. Do not hand-edit; edit the spec or the "
        "script's OVERRIDES and rerun --write.",
        "spec": "discussions/spec/language.md",
        "counts": counts,
        "blocks": blocks,
    }


def do_write(blocks, fixtures):
    manifest = build_manifest(blocks)
    MANIFEST.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n")
    for rel, text in fixtures.items():
        path = LANG_DIR / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    print(
        f"wrote manifest ({len(blocks)} blocks) + {len(fixtures)} fixtures; "
        f"{manifest['counts']}"
    )


def do_check(blocks, fixtures):
    problems = []
    committed = load_manifest()
    expected = build_manifest(blocks)
    if committed != expected:
        if committed.get("blocks") != expected["blocks"]:
            problems.append(
                "manifest `blocks` differs from the spec — a block was added, "
                "removed, reordered, edited, or retagged. Diff:"
            )
            problems += diff_blocks(committed.get("blocks", []), expected["blocks"])
        else:
            problems.append(
                "manifest metadata (counts/note/spec) does not match the spec — "
                "regenerate; do not hand-edit the manifest."
            )
    for rel, text in fixtures.items():
        path = LANG_DIR / rel
        if not path.exists():
            problems.append(f"missing fixture: {rel}")
        elif path.read_text() != text:
            problems.append(f"fixture out of sync with spec: {rel}")
    on_disk = {
        str(p.relative_to(LANG_DIR)).replace("\\", "/")
        for p in LANG_DIR.rglob("spec-b*.doodle")
    }
    for orphan in sorted(on_disk - set(fixtures)):
        problems.append(f"orphan fixture (no spec block): {orphan}")
    if problems:
        print("lang corpus is OUT OF SYNC with the spec:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        print("Run scripts/lang-corpus-sync.py --write and review.", file=sys.stderr)
        sys.exit(1)
    golden = sum(1 for b in blocks if b.get("disposition") in ("golden", "wrapped"))
    print(f"lang corpus in sync: {len(blocks)} blocks, {golden} golden fixtures.")


def diff_blocks(committed, current):
    by_i_c = {b["index"]: b for b in committed}
    by_i_n = {b["index"]: b for b in current}
    out = []
    for i in sorted(set(by_i_c) | set(by_i_n)):
        c, n = by_i_c.get(i), by_i_n.get(i)
        if c != n:
            out.append(f"    block #{i}: committed={c} spec={n}")
    return out or ["    (order changed)"]


def main():
    ap = argparse.ArgumentParser(description="Sync the golden language corpus with the spec.")
    ap.add_argument("--write", action="store_true", help="regenerate manifest + fixtures")
    args = ap.parse_args()
    if not SPEC.exists():
        die(f"spec not found at {SPEC} (run in the workspace, with `discussions` checked out)")
    blocks, fixtures = plan_corpus(SPEC.read_text())
    if args.write:
        do_write(blocks, fixtures)
    else:
        do_check(blocks, fixtures)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Merge lcov tracefiles from simulator runs (`trmnl-sim --coverage`) and report on them.

    scripts/coverage.py cov/                       # summary of every *.info in cov/
    scripts/coverage.py cov/ -o all.info --include src/ --include lib/
    scripts/coverage.py a.info b.info --html cov-html --root ../trmnl-firmware

Inputs are tracefiles or directories of them. Line and function hit counts are summed
(a simulator records 0 or 1 per line, so a merged count is the number of runs that
executed it). `--include` keeps only files whose path starts with a prefix; paths are
relative to the firmware checkout for its own sources (see `--coverage-root`).

The summary lists every file with its line and function coverage, then the total.
`--html DIR` writes a self-contained report (an index plus one page per file with
the source colored by coverage; sources are read from `--root`). `genhtml` from lcov
also reads the merged tracefile, if it is installed.

Standard library only.
"""

from __future__ import annotations

import argparse
import html
import sys
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class FileCov:
    lines: dict[int, int] = field(default_factory=dict)
    # name -> [line, hits]
    functions: dict[str, list[int]] = field(default_factory=dict)

    @property
    def lines_hit(self) -> int:
        return sum(1 for n in self.lines.values() if n)

    @property
    def functions_hit(self) -> int:
        return sum(1 for _, n in self.functions.values() if n)


Coverage = dict[str, FileCov]

# What tests/integration/run.py names its merge of a coverage directory.
MERGED = "merged.info"


def tracefiles(inputs: list[str], exclude: Path | None = None) -> list[Path]:
    out: list[Path] = []
    for i in inputs:
        p = Path(i)
        # A directory means the per-simulator tracefiles, not an earlier merge of them.
        found = sorted(f for f in p.glob("*.info") if f.name != MERGED) if p.is_dir() else [p]
        out += [f for f in found if exclude is None or f.resolve() != exclude.resolve()]
    return out


def parse(path: Path, into: Coverage, include: list[str]) -> None:
    cur: FileCov | None = None
    for line in path.read_text(errors="replace").splitlines():
        key, _, val = line.partition(":")
        if key == "SF":
            cur = into.setdefault(val, FileCov()) if not include or val.startswith(tuple(include)) else None
        elif cur is None:
            continue
        elif key == "DA":
            ln, count = val.split(",")[:2]
            cur.lines[int(ln)] = cur.lines.get(int(ln), 0) + int(count)
        elif key == "FN":
            # FN:<line>,<name> (lcov 2 may add an end line: FN:<line>,<end>,<name>)
            parts = val.split(",", 2)
            name = parts[2] if len(parts) == 3 and parts[1].isdigit() else val.split(",", 1)[1]
            cur.functions.setdefault(name, [int(parts[0]), 0])
        elif key == "FNDA":
            count, name = val.split(",", 1)
            cur.functions.setdefault(name, [0, 0])[1] += int(count)
        elif key == "end_of_record":
            cur = None


def write_lcov(cov: Coverage, path: Path) -> None:
    out = ["TN:"]
    for name, f in sorted(cov.items()):
        out.append(f"SF:{name}")
        funcs = sorted(f.functions.items(), key=lambda kv: (kv[1][0], kv[0]))
        out += [f"FN:{ln},{fn}" for fn, (ln, _) in funcs]
        out += [f"FNDA:{n},{fn}" for fn, (_, n) in funcs]
        out += [f"FNF:{len(funcs)}", f"FNH:{f.functions_hit}"]
        out += [f"DA:{ln},{n}" for ln, n in sorted(f.lines.items())]
        out += [f"LF:{len(f.lines)}", f"LH:{f.lines_hit}", "end_of_record"]
    path.write_text("\n".join(out) + "\n")


def pct(hit: int, found: int) -> str:
    return f"{100 * hit / found:5.1f}%" if found else "    -"


def totals(cov: Coverage) -> tuple[int, int, int, int]:
    lh = sum(f.lines_hit for f in cov.values())
    lf = sum(len(f.lines) for f in cov.values())
    fh = sum(f.functions_hit for f in cov.values())
    ff = sum(len(f.functions) for f in cov.values())
    return lh, lf, fh, ff


def summary(cov: Coverage) -> str:
    width = max([len(n) for n in cov] + [5])
    rows = [f"{'file':<{width}}  {'lines':>15}  {'functions':>13}"]
    for name, f in sorted(cov.items()):
        lf, ff = len(f.lines), len(f.functions)
        rows.append(
            f"{name:<{width}}  {f.lines_hit:>5}/{lf:<5} {pct(f.lines_hit, lf)}  "
            f"{f.functions_hit:>4}/{ff:<4} {pct(f.functions_hit, ff)}"
        )
    lh, lf, fh, ff = totals(cov)
    rows.append(f"{'total':<{width}}  {lh:>5}/{lf:<5} {pct(lh, lf)}  {fh:>4}/{ff:<4} {pct(fh, ff)}")
    return "\n".join(rows)


CSS = """
body { font: 14px/1.4 -apple-system, system-ui, sans-serif; margin: 16px; color: #1d1d1f; background: #fff; }
table { border-collapse: collapse; }
td, th { padding: 2px 10px; text-align: right; }
td:first-child, th:first-child { text-align: left; }
tr:nth-child(even) { background: #f4f4f6; }
.bar { display: inline-block; width: 80px; height: 8px; background: #e5484d; vertical-align: middle; }
.bar i { display: block; height: 100%; background: #30a46c; }
pre { margin: 0; font: 12px/1.45 ui-monospace, Menlo, monospace; }
.src td { padding: 0 8px; text-align: left; white-space: pre; font: 12px/1.45 ui-monospace, Menlo, monospace; }
.src td.n { text-align: right; color: #888; user-select: none; }
.hit { background: #d8f5e3; } .miss { background: #fbdcdc; }
@media (prefers-color-scheme: dark) {
  body { color: #e8e8ea; background: #1b1b1f; } tr:nth-child(even) { background: #25252a; }
  .hit { background: #1d3b2a; } .miss { background: #4a2126; }
}
"""


def page(title: str, body: str) -> str:
    return (
        f"<!doctype html><html><head><meta charset='utf-8'><title>{html.escape(title)}</title>"
        f"<meta name='viewport' content='width=device-width'><style>{CSS}</style></head>"
        f"<body>{body}</body></html>\n"
    )


def bar(hit: int, found: int) -> str:
    w = 100 * hit / found if found else 0
    return f"<span class='bar'><i style='width:{w:.0f}%'></i></span>"


def write_html(cov: Coverage, out: Path, root: Path) -> None:
    out.mkdir(parents=True, exist_ok=True)
    rows = []
    for i, (name, f) in enumerate(sorted(cov.items())):
        lf, ff = len(f.lines), len(f.functions)
        link = f"f{i}.html"
        rows.append(
            f"<tr><td><a href='{link}'>{html.escape(name)}</a></td><td>{bar(f.lines_hit, lf)}</td>"
            f"<td>{pct(f.lines_hit, lf)}</td><td>{f.lines_hit}/{lf}</td><td>{pct(f.functions_hit, ff)}</td>"
            f"<td>{f.functions_hit}/{ff}</td></tr>"
        )
        src = Path(name) if Path(name).is_absolute() else root / name
        try:
            text = src.read_text(errors="replace").splitlines()
        except OSError:
            text = []
        lines = []
        for n in range(1, max([len(text)] + list(f.lines)) + 1):
            cls = {None: "", 0: " class='miss'"}.get(f.lines.get(n), " class='hit'")
            code = html.escape(text[n - 1]) if n <= len(text) else ""
            lines.append(f"<tr{cls}><td class='n'>{n}</td><td class='n'>{f.lines.get(n, '')}</td><td>{code}</td></tr>")
        funcs = "".join(
            f"<tr><td>{html.escape(fn)}</td><td>{ln}</td><td>{n}</td></tr>"
            for fn, (ln, n) in sorted(f.functions.items(), key=lambda kv: kv[1][0])
        )
        missing = "" if text else f"<p>Source not found at {html.escape(str(src))}.</p>"
        (out / link).write_text(
            page(
                name,
                f"<p><a href='index.html'>index</a></p><h2>{html.escape(name)}</h2>"
                f"<p>lines {f.lines_hit}/{lf} {pct(f.lines_hit, lf)}, functions {f.functions_hit}/{ff}</p>"
                f"<table><tr><th>function</th><th>line</th><th>runs</th></tr>{funcs}</table><br>{missing}"
                f"<table class='src'>{''.join(lines)}</table>",
            )
        )
    lh, lf, fh, ff = totals(cov)
    (out / "index.html").write_text(
        page(
            "Firmware coverage",
            f"<h2>Firmware coverage</h2><p>lines {lh}/{lf} {pct(lh, lf)}, functions {fh}/{ff} {pct(fh, ff)}</p>"
            "<table><tr><th>file</th><th></th><th>lines</th><th></th><th>functions</th><th></th></tr>"
            + "".join(rows)
            + "</table>",
        )
    )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("inputs", nargs="+", help="lcov tracefiles or directories of *.info")
    ap.add_argument("-o", "--output", type=Path, help="write the merged tracefile here")
    ap.add_argument("--include", action="append", default=[], metavar="PREFIX", help="only files starting with PREFIX")
    ap.add_argument("--html", type=Path, metavar="DIR", help="write an HTML report here")
    ap.add_argument("--root", type=Path, default=Path("."), help="where relative source paths are (for --html)")
    ap.add_argument("-q", "--quiet", action="store_true", help="print only the total")
    args = ap.parse_args(argv)

    cov: Coverage = {}
    files = tracefiles(args.inputs, exclude=args.output)
    if not files:
        print("no tracefiles found", file=sys.stderr)
        return 1
    for f in files:
        parse(f, cov, args.include)
    if args.output:
        write_lcov(cov, args.output)
    if args.html:
        write_html(cov, args.html, args.root)
    if args.quiet:
        lh, lf, fh, ff = totals(cov)
        print(f"lines {lh}/{lf} {pct(lh, lf).strip()}, functions {fh}/{ff} {pct(fh, ff).strip()} "
              f"in {len(cov)} files ({len(files)} tracefiles)")
    else:
        print(summary(cov))
        print(f"({len(files)} tracefiles)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

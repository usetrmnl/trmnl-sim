"""`python -m unittest -v` with colored results.

    python3 run.py                  # discover and run every test_*.py
    python3 run.py test_trmnl_x     # any unittest selectors
    python3 run.py -j 1             # one at a time, in this process
    python3 run.py --no-cache       # build the devices the tests start from afresh

Each selector (by default, each test_*.py) runs in its own worker process, up to -j N
(default: the number of CPUs) at a time, slowest modules first. A module that sets
PARALLEL_BY_CLASS = True (its fixtures are built lazily, see support.fixture) is split
further: each of its classes gets a worker. A worker's output is
printed in one piece when it finishes, followed by the combined verdict. A single
selector, or -j 1, runs in-process with the output streamed as usual.

The devices the tests start from (a factory-fresh X, onboarded devices) come from an
on-disk cache keyed by the firmware, the simulator and the test support code; see
setup_cache.py. --no-cache (TRMNL_SPEC_NO_CACHE=1) bypasses it, running every setup flow in
full, as CI does.

Color is on when stderr is a terminal; NO_COLOR=1 turns it off, FORCE_COLOR=1 turns it on
(e.g. in CI logs, which render ANSI colors).

With TRMNL_SIM_COVERAGE=<dir>, every simulator writes an lcov tracefile there; afterwards
they are merged into <dir>/merged.info and <dir>/html/, and the coverage of the
firmware's own src/ and lib/ is printed.
"""

import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent

GREEN, RED, YELLOW, BOLD, RESET = "\033[32m", "\033[31m", "\033[33m", "\033[1m", "\033[0m"


def use_color(stream) -> bool:
    if os.environ.get("NO_COLOR"):
        return False
    if os.environ.get("FORCE_COLOR"):
        return True
    return hasattr(stream, "isatty") and stream.isatty()


COLOR = use_color(sys.stderr)


def paint(color: str, text: str) -> str:
    return f"{color}{text}{RESET}" if COLOR else text


class ColorResult(unittest.TextTestResult):
    def _report(self, color: str, word: str, dot: str) -> None:
        if self.showAll:
            self.stream.writeln(paint(color, word))
        elif self.dots:
            self.stream.write(paint(color, dot))
        self.stream.flush()

    def addSuccess(self, test):
        unittest.TestResult.addSuccess(self, test)
        self._report(GREEN, "ok", ".")

    def addError(self, test, err):
        unittest.TestResult.addError(self, test, err)
        self._report(RED + BOLD, "ERROR", "E")

    def addFailure(self, test, err):
        unittest.TestResult.addFailure(self, test, err)
        self._report(RED + BOLD, "FAIL", "F")

    def addSkip(self, test, reason):
        unittest.TestResult.addSkip(self, test, reason)
        self._report(YELLOW, f"skipped {reason!r}", "s")

    def addExpectedFailure(self, test, err):
        unittest.TestResult.addExpectedFailure(self, test, err)
        self._report(YELLOW, "expected failure", "x")

    def addUnexpectedSuccess(self, test):
        unittest.TestResult.addUnexpectedSuccess(self, test)
        self._report(RED + BOLD, "unexpected success", "u")

    def printErrorList(self, flavour, errors):
        super().printErrorList(paint(RED + BOLD, flavour), errors)


class ColorStream:
    """Colors the runner's closing "OK" / "FAILED" verdict."""

    def __init__(self, stream):
        self._stream = stream

    def write(self, text):
        if text == "OK":
            text = paint(GREEN + BOLD, text)
        elif text == "FAILED":
            text = paint(RED + BOLD, text)
        return self._stream.write(text)

    def __getattr__(self, name):
        return getattr(self._stream, name)


class ColorRunner(unittest.TextTestRunner):
    resultclass = ColorResult

    def __init__(self, *args, **kw):
        kw["stream"] = ColorStream(kw.get("stream") or sys.stderr)
        super().__init__(*args, **kw)


def report_coverage(cov_dir: str) -> None:
    here = Path(__file__).resolve().parent
    sys.path.insert(0, str(here.parent.parent / "scripts"))
    import coverage

    out = Path(cov_dir)
    # Relative paths in the tracefiles are relative to the firmware checkout.
    build = os.environ.get("TRMNL_FIRMWARE_BUILD", str(here.parent.parent.parent / "trmnl-firmware/.pio/build/trmnl"))
    root = Path(build).resolve().parent.parent.parent
    print(paint(BOLD, f"\nFirmware coverage (src/, lib/); full report in {out / 'html'}"), file=sys.stderr)
    coverage.main([str(out), "-o", str(out / "merged.info"), "--html", str(out / "html"), "--root", str(root), "-q"])
    coverage.main([str(out), "--include", "src/", "--include", "lib/"])


# Started first so the longest ones don't end up running alone at the end (slowest first,
# from a full run); anything not listed follows in name order.
SLOW_FIRST = ["test_trmnl_x", "test_faults_x", "test_memcheck", "test_faults", "test_refresh_cycle"]

COUNTS = ("run", "failures", "errors", "skipped", "expected_failures", "unexpected_successes")


def run_here(argv: list[str]) -> bool:
    """unittest.main on `argv`; writes the counts to $TRMNL_SPEC_RESULT if set."""
    prog = unittest.main(module=None, argv=["run.py", *argv], testRunner=ColorRunner, exit=False)
    r = prog.result
    out = os.environ.get("TRMNL_SPEC_RESULT")
    if out:
        counts = [r.testsRun, len(r.failures), len(r.errors), len(r.skipped), len(r.expectedFailures), len(r.unexpectedSuccesses)]
        Path(out).write_text(json.dumps(dict(zip(COUNTS, counts))))
    return r.wasSuccessful()


def parse_args(argv: list[str]) -> tuple[int, list[str], list[str]]:
    """Split off -j N / --jobs N; the rest is unittest flags and selectors."""
    jobs, flags, selectors = os.cpu_count() or 1, [], []
    it = iter(argv)
    for a in it:
        if a in ("-j", "--jobs"):
            jobs = int(next(it))
        elif a.startswith("-j") and a[2:].isdigit():
            jobs = int(a[2:])
        elif a.startswith("--jobs="):
            jobs = int(a.split("=", 1)[1])
        elif a == "--no-cache":
            os.environ["TRMNL_SPEC_NO_CACHE"] = "1"  # read by setup_cache, here and in workers
        elif a.startswith("-"):
            flags.append(a)
            if a in ("-k", "-p", "--pattern", "-s", "--start-directory", "-t", "--top-level-directory"):
                flags.append(next(it))
        else:
            selectors.append(a)
    return max(1, jobs), flags, selectors


def split_by_class(unit: str) -> list[str]:
    """`unit`'s test classes if it is a module with PARALLEL_BY_CLASS, else just `unit`."""
    if "." in unit:
        return [unit]
    import importlib

    sys.path.insert(0, str(HERE))
    module = importlib.import_module(unit)
    if not getattr(module, "PARALLEL_BY_CLASS", False):
        return [unit]
    classes: list[str] = []

    def walk(suite):
        for t in suite:
            if isinstance(t, unittest.TestSuite):
                walk(t)
            elif type(t).__name__ not in classes:
                classes.append(type(t).__name__)

    walk(unittest.defaultTestLoader.loadTestsFromModule(module))
    return [f"{unit}.{c}" for c in classes]


def run_parallel(jobs: int, flags: list[str], units: list[str]) -> bool:
    env = dict(os.environ)
    if COLOR:
        env["FORCE_COLOR"] = "1"
    tmp = Path(tempfile.mkdtemp(prefix="trmnl-spec-"))
    pending, running, totals, failed = list(units), {}, dict.fromkeys(COUNTS, 0), []
    t0 = time.monotonic()
    print(paint(BOLD, f"Running {len(units)} test groups, {min(jobs, len(units))} at a time"), file=sys.stderr, flush=True)
    try:
        while pending or running:
            while pending and len(running) < jobs:
                unit = pending.pop(0)
                result, log = tmp / f"{unit}.json", tmp / f"{unit}.log"
                fh = open(log, "w+")
                proc = subprocess.Popen(
                    [sys.executable, __file__, "-j", "1", *flags, unit],
                    cwd=HERE, env={**env, "TRMNL_SPEC_RESULT": str(result)},
                    stdout=fh, stderr=subprocess.STDOUT, start_new_session=True,
                )
                running[proc] = (unit, result, fh, time.monotonic())
            time.sleep(0.2)
            for proc in [p for p in running if p.poll() is not None]:
                unit, result, fh, start = running.pop(proc)
                fh.seek(0)
                output = fh.read()
                fh.close()
                counts = json.loads(result.read_text()) if result.exists() else None
                ok = proc.returncode == 0 and counts is not None
                if counts:
                    for k in COUNTS:
                        totals[k] += counts[k]
                if not ok:
                    failed.append(unit)
                took = time.monotonic() - start
                mark = paint(GREEN, "ok") if ok else paint(RED + BOLD, "FAILED")
                print(paint(BOLD, f"\n=== {unit} ({took:.0f}s) ") + mark, file=sys.stderr)
                sys.stderr.write(output)
                sys.stderr.flush()
    except KeyboardInterrupt:
        for proc in running:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        raise
    finally:
        for proc in running:
            proc.wait()

    elapsed = time.monotonic() - t0
    print("\n" + "=" * 70, file=sys.stderr)
    print(f"Ran {totals['run']} tests in {elapsed:.1f}s ({len(units)} groups, -j {jobs})\n", file=sys.stderr)
    details = [f"{k.replace('_', ' ')}={totals[k]}" for k in COUNTS[1:] if totals[k]]
    if failed:
        print(paint(RED + BOLD, "FAILED") + f" ({', '.join(details)}) in: {', '.join(failed)}", file=sys.stderr)
    else:
        print(paint(GREEN + BOLD, "OK") + (f" ({', '.join(details)})" if details else ""), file=sys.stderr)
    return not failed


if __name__ == "__main__":
    jobs, flags, selectors = parse_args(sys.argv[1:])
    units = selectors or sorted((p.stem for p in HERE.glob("test_*.py")), key=lambda m: (m not in SLOW_FIRST, SLOW_FIRST.index(m) if m in SLOW_FIRST else 0, m))
    if jobs == 1 or len(units) == 1:
        ok = run_here([*flags, *selectors])
    else:
        ok = run_parallel(jobs, flags, [u for unit in units for u in split_by_class(unit)])
    # Workers only write tracefiles; the top-level run merges and reports them.
    if os.environ.get("TRMNL_SIM_COVERAGE") and not os.environ.get("TRMNL_SPEC_RESULT"):
        report_coverage(os.environ["TRMNL_SIM_COVERAGE"])
    sys.exit(not ok)

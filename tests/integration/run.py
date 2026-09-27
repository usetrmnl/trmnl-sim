"""`python -m unittest -v` with colored results.

    python3 run.py                  # discover and run every test_*.py
    python3 run.py test_trmnl_x     # any unittest selectors
    python3 run.py -j 1             # one at a time, in this process
    python3 run.py --no-cache       # build the devices the tests start from afresh
    python3 run.py xteink_x4        # every test of a PlatformIO environment (or --env NAME)
    python3 run.py --list-envs      # the environments and how many tests each has
    python3 run.py --comprehensive  # also the full general suite on a device per family
    python3 run.py --exhaustive     # the full general suite on every device
    python3 run.py --dry-run ...    # print what would run
    python3 run.py --slow ...       # also the tests marked @slow (support.slow)

With no selectors, every device's own tests run, the general tests (ENV = ANY) in full on the
TRMNL OG, and devices.SMOKE (one general test per area) on every other device.
--comprehensive runs the full general suite on devices.REPRESENTATIVES (one per family of
devices sharing chip, panel controller and inks) instead of the smoke tests;
--exhaustive, on every device.

Every test class says which PlatformIO environment's build it runs: its `ENV` attribute,
else its module's `ENV`. A selector that isn't a test module (test_*) is an environment
name and stands for all of that environment's test classes.

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


class DeviceLoader(unittest.TestLoader):
    """Loads tests with the device under test's known failures marked: a module's
    KNOWN_FAILURES = {"<env>": {"Class.test_name" or "Class": "what the firmware gets wrong"}}
    turns those tests into expected failures when that environment is under test."""

    def loadTestsFromNames(self, names, module=None):
        return self._mark(super().loadTestsFromNames(names, module))

    def loadTestsFromModule(self, module, *args, **kw):
        return self._mark(super().loadTestsFromModule(module, *args, **kw))

    @staticmethod
    def _mark(suite):
        from devices import under_test

        env = under_test().env

        def walk(s):
            for t in s:
                if isinstance(t, unittest.TestSuite):
                    walk(t)
                    continue
                cls = type(t)
                known = getattr(sys.modules.get(cls.__module__), "KNOWN_FAILURES", {}).get(env, {})
                name = getattr(t, "_testMethodName", None)
                if name and (f"{cls.__name__}.{name}" in known or cls.__name__ in known):
                    fn = getattr(cls, name)
                    if not getattr(fn, "__unittest_expecting_failure__", False):
                        setattr(cls, name, unittest.expectedFailure(fn))

        walk(suite)
        return suite


def run_here(argv: list[str]) -> bool:
    """unittest.main on `argv`; writes the counts to $TRMNL_SPEC_RESULT if set."""
    prog = unittest.main(module=None, argv=["run.py", *argv], testRunner=ColorRunner, testLoader=DeviceLoader(),
                         exit=False)
    r = prog.result
    out = os.environ.get("TRMNL_SPEC_RESULT")
    if out:
        counts = [r.testsRun, len(r.failures), len(r.errors), len(r.skipped), len(r.expectedFailures), len(r.unexpectedSuccesses)]
        Path(out).write_text(json.dumps(dict(zip(COUNTS, counts))))
    return r.wasSuccessful()


def parse_args(argv: list[str]) -> tuple[int, list[str], list[str]]:
    """Split off -j N / --jobs N; the rest is unittest flags and selectors (--env NAME
    becomes the selector NAME)."""
    jobs, flags, selectors = os.cpu_count() or 1, [], []
    it = iter(argv)
    for a in it:
        if a == "--env":
            selectors.append(next(it))
        elif a.startswith("--env="):
            selectors.append(a.split("=", 1)[1])
        elif a in ("-j", "--jobs"):
            jobs = int(next(it))
        elif a.startswith("-j") and a[2:].isdigit():
            jobs = int(a[2:])
        elif a.startswith("--jobs="):
            jobs = int(a.split("=", 1)[1])
        elif a == "--slow":
            os.environ["TRMNL_SIM_SLOW"] = "1"  # read by support.slow, here and in workers
        elif a == "--no-cache":
            os.environ["TRMNL_SPEC_NO_CACHE"] = "1"  # read by setup_cache, here and in workers
        elif a.startswith("-"):
            flags.append(a)
            if a in ("-k", "-p", "--pattern", "-s", "--start-directory", "-t", "--top-level-directory"):
                flags.append(next(it))
        else:
            selectors.append(a)
    return max(1, jobs), flags, selectors


def test_modules() -> list[str]:
    return sorted(p.stem for p in HERE.glob("test_*.py"))


def is_env(selector: str) -> bool:
    """A selector that names a PlatformIO environment rather than tests."""
    return not selector.startswith("test_") and "." not in selector


TEST_COUNTS: dict[tuple[str, str], int] = {}


def classes_by_env() -> dict[str, list[tuple[str, str]]]:
    """Every test class as (module, class), by the environment it runs (`ENV` on the class,
    else on its module). Exits listing any class that declares none."""
    import importlib

    sys.path.insert(0, str(HERE))
    out: dict[str, list[tuple[str, str]]] = {}
    missing = []
    TEST_COUNTS.clear()
    for name in test_modules():
        module = importlib.import_module(name)
        seen = set()

        def walk(suite):
            for t in suite:
                if isinstance(t, unittest.TestSuite):
                    walk(t)
                    continue
                if isinstance(t, unittest.loader._FailedTest):
                    continue
                key = (name, type(t).__name__)
                TEST_COUNTS[key] = TEST_COUNTS.get(key, 0) + 1
                if type(t) in seen:
                    continue
                seen.add(type(t))
                env = getattr(type(t), "ENV", None) or getattr(module, "ENV", None)
                if env:
                    out.setdefault(env, []).append(key)
                else:
                    missing.append(f"{name}.{type(t).__name__}")

        walk(unittest.defaultTestLoader.loadTestsFromModule(module))
    from devices import ANY, DEVICES

    strays = sorted(e for e in out if e != ANY and e not in DEVICES)
    if strays:
        sys.exit(f"tests name environments devices.py doesn't know: {', '.join(strays)}")
    if missing:
        sys.exit(f"these test classes don't say which PlatformIO environment they run (set ENV on "
                 f"the class or its module): {', '.join(missing)}")
    return out


def units_of(classes: set[tuple[str, str]], by_env: dict[str, list[tuple[str, str]]]) -> list[str]:
    """Run units for a set of test classes: a whole module when all of it is in the set and
    it keeps its fixtures module-wide (no PARALLEL_BY_CLASS), else its classes."""
    import importlib

    units = []
    for name in test_modules():
        mine = {c for m, c in classes if m == name}
        if not mine:
            continue
        everything = [c for cs in by_env.values() for m, c in cs if m == name]
        module = importlib.import_module(name)
        if len(mine) == len(everything) and not getattr(module, "PARALLEL_BY_CLASS", False):
            units.append(name)
        else:
            units += [f"{name}.{c}" for c in everything if c in mine]
    return units


def env_units(envs: list[str]) -> list[tuple[str, str]]:
    """(selector, environment) run units for environments: the tests specific to each, plus
    the general ones (ENV = ANY) with it as the device under test. Exits if an environment
    is unknown or not built."""
    from devices import ANY, device
    from support import build_for_env

    by_env = classes_by_env()
    try:
        envs = [device(e).env for e in envs]  # PlatformIO names are case-sensitive; accept trmnl_x
    except KeyError as e:
        sys.exit(f"{e.args[0]}; test modules are named test_*")
    units = []
    for e in envs:
        build = build_for_env(e)
        if not (build / "firmware.elf").exists():
            sys.exit(f"no {e} build at {build}: run `pio run -e {e}` in the firmware checkout "
                     f"(or bin/spec --build-firmware {e})")
        classes = set(by_env.get(e, []))
        why = device(e).general
        if why:
            print(paint(YELLOW, f"{e}: the general tests don't run: {why}"), file=sys.stderr)
        else:
            classes |= set(by_env.get(ANY, []))
        units += [(u, e) for u in units_of(classes, by_env)]
    return units


def list_envs() -> None:
    from support import build_for_env

    from devices import ANY, DEVICES, REPRESENTATIVES, SMOKE

    by_env = classes_by_env()
    general = sum(TEST_COUNTS.get(c, 0) for c in by_env.get(ANY, []))
    print(f"{general} general tests; bin/spec runs them in full on the OG and {len(SMOKE)} smoke tests on the other "
          f"devices, --comprehensive in full on the devices marked *, --exhaustive in full everywhere\n")
    print(f" {'environment':31} {'own':>5} {'total':>5}  build")
    for env in sorted(DEVICES, key=str.lower):
        built = "yes" if (build_for_env(env) / "firmware.elf").exists() else "missing"
        own = sum(TEST_COUNTS.get(c, 0) for c in by_env.get(env, []))
        why = DEVICES[env].general
        total = own if why else own + general
        mark = "*" if env in REPRESENTATIVES else " "
        print(f"{mark}{env:31} {own:>5} {total:>5}  {built}" + (f"  (no general tests: {why})" if why else ""))


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


def unit_name(unit) -> str:
    if not isinstance(unit, tuple):
        return unit
    sel, env = unit
    if isinstance(sel, tuple):
        sel = f"{sel[0].split('.')[0]} smoke"
    return f"{sel} [{env}]"


def plan(tier: str) -> list:
    """Run units for the whole suite at `tier` (standard, comprehensive or exhaustive)."""
    from devices import ANY, DEVICES, REPRESENTATIVES, SMOKE

    by_env = classes_by_env()
    missing = [t for t in SMOKE if not _test_exists(t)]
    if missing:
        sys.exit(f"devices.SMOKE names tests that don't exist: {', '.join(missing)}")
    modules = sorted(test_modules(), key=lambda m: (m not in SLOW_FIRST, SLOW_FIRST.index(m) if m in SLOW_FIRST else 0, m))
    units: list = [u for m in modules for u in split_by_class(m)]  # general ones on the OG
    general = set(by_env.get(ANY, []))
    for env, d in DEVICES.items():
        if env == "trmnl" or d.general:
            continue
        if tier == "exhaustive" or (tier == "comprehensive" and env in REPRESENTATIVES):
            units += [(u, env) for u in units_of(general, by_env)]
        else:
            per_module: dict[str, list[str]] = {}
            for t in SMOKE:
                per_module.setdefault(t.split(".")[0], []).append(t)
            units += [(tuple(ts), env) for ts in per_module.values()]
    return units


def _test_exists(test_id: str) -> bool:
    import importlib

    module, cls, name = test_id.split(".")
    try:
        return hasattr(getattr(importlib.import_module(module), cls), name)
    except (ImportError, AttributeError):
        return False


def run_parallel(jobs: int, flags: list[str], units: list) -> bool:
    """Run each unit (a selector, or (selector, environment of the device under test)) in a
    worker process."""
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
                selector, device = unit if isinstance(unit, tuple) else (unit, None)
                selectors = list(selector) if isinstance(selector, tuple) else [selector]
                stem = unit_name(unit).replace(" ", "")
                result, log = tmp / f"{stem}.json", tmp / f"{stem}.log"
                fh = open(log, "w+")
                wenv = {**env, "TRMNL_SPEC_RESULT": str(result)}
                if device:
                    wenv["TRMNL_SIM_DEVICE"] = device
                proc = subprocess.Popen(
                    [sys.executable, __file__, "-j", "1", *flags, *selectors],
                    cwd=HERE, env=wenv,
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
                    failed.append(unit_name(unit))
                took = time.monotonic() - start
                mark = paint(GREEN, "ok") if ok else paint(RED + BOLD, "FAILED")
                print(paint(BOLD, f"\n=== {unit_name(unit)} ({took:.0f}s) ") + mark, file=sys.stderr)
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
    tier = "exhaustive" if "--exhaustive" in flags else "comprehensive" if "--comprehensive" in flags else "standard"
    dry_run = "--dry-run" in flags
    flags = [f for f in flags if f not in ("--exhaustive", "--comprehensive", "--dry-run")]
    if not selectors and not os.environ.get("TRMNL_SPEC_RESULT") and "--list-envs" not in flags:
        units = plan(tier)
        if dry_run:
            for u in units:
                print(unit_name(u) + (f": {' '.join(u[0])}" if isinstance(u, tuple) and isinstance(u[0], tuple) else ""))
            print(f"{len(units)} test groups ({tier})")
            sys.exit(0)
        ok = run_parallel(jobs, flags, units)
        if os.environ.get("TRMNL_SIM_COVERAGE"):
            report_coverage(os.environ["TRMNL_SIM_COVERAGE"])
        sys.exit(not ok)
    if "--list-envs" in flags:
        list_envs()
        sys.exit(0)
    envs = [s for s in selectors if is_env(s)]
    if envs:
        units = [u for s in selectors if not is_env(s) for u in split_by_class(s)] + env_units(envs)
        if dry_run:
            for u in units:
                print(unit_name(u))
            sys.exit(0)
        if jobs == 1:
            # one environment at a time, each in-process run with its device under test
            ok = True
            for u in units:
                selector, device = u if isinstance(u, tuple) else (u, None)
                code = subprocess.call([sys.executable, __file__, "-j", "1", *flags, selector], cwd=HERE,
                                       env={**os.environ, **({"TRMNL_SIM_DEVICE": device} if device else {})})
                ok = ok and code == 0
        else:
            ok = run_parallel(jobs, flags, units)
        if os.environ.get("TRMNL_SIM_COVERAGE") and not os.environ.get("TRMNL_SPEC_RESULT"):
            report_coverage(os.environ["TRMNL_SIM_COVERAGE"])
        sys.exit(not ok)
    units = selectors or sorted((p.stem for p in HERE.glob("test_*.py")), key=lambda m: (m not in SLOW_FIRST, SLOW_FIRST.index(m) if m in SLOW_FIRST else 0, m))
    if jobs == 1 or len(units) == 1:
        ok = run_here([*flags, *selectors])
    else:
        ok = run_parallel(jobs, flags, [u for unit in units for u in split_by_class(unit)])
    # Workers only write tracefiles; the top-level run merges and reports them.
    if os.environ.get("TRMNL_SIM_COVERAGE") and not os.environ.get("TRMNL_SPEC_RESULT"):
        report_coverage(os.environ["TRMNL_SIM_COVERAGE"])
    sys.exit(not ok)

"""`python -m unittest -v` with colored results.

    python3 run.py                  # discover and run every test_*.py
    python3 run.py test_trmnl_x     # any unittest selectors

Color is on when stderr is a terminal; NO_COLOR=1 turns it off, FORCE_COLOR=1 turns it on
(e.g. in CI logs, which render ANSI colors).
"""

import os
import sys
import unittest

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


if __name__ == "__main__":
    sys.argv[0] = "run.py"
    unittest.main(module=None, testRunner=ColorRunner)

"""HTTPS on the TRMNL OG against the mock server with TLS (ECDHE-ECDSA, P-384): plain
WiFiClientSecure for other servers, and for trmnl.app the resumable client that keeps its
TLS session in RTC memory across deep sleep."""

import unittest

from support import MockTrmnl, sim

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker


def onboard(mock: MockTrmnl, *extra_args: str):
    """A fresh OG joined to `mock` through the portal; returns it asleep after its first refresh."""
    s = sim(erase=True, extra_args=("--offline", *extra_args))
    try:
        s.wait(portal=True, timeout_s=40)
        s.portal_connect("TRMNL-Sim", "any-password", server=mock.device_url)
        mock.wait_for_request("/api/display", timeout_s=40)
        s.wait(state="deep_sleep", timeout_s=30)
    except BaseException:
        s.close()
        raise
    return s


def refresh(s, mock: MockTrmnl, wake=None) -> list:
    """Wake the device (by its timer, or `wake`) and return the requests of that refresh."""
    n = len(mock.requests)
    (wake or s.wake)()
    mock.wait_for_request("/api/display", after=n, timeout_s=30)
    s.wait(state="deep_sleep", timeout_s=30)
    return mock.requests[n:]


class OtherServer(unittest.TestCase):
    def test_onboarding_and_refreshes_over_https(self):
        with MockTrmnl(tls=True) as mock, onboard(mock) as s:
            self.assertEqual([r.path for r in mock.requests],
                             ["/api/setup", "/images/default.bmp", "/api/log", "/api/display",
                              "/images/default.bmp"])
            self.assertEqual(mock.requests[3].headers["Access-Token"], mock.api_key)
            # a full handshake every time: only trmnl.app sessions are resumed
            self.assertEqual({r.tls_resumed for r in refresh(s, mock)}, {False})


class TrmnlApp(unittest.TestCase):
    def test_tls_session_is_resumed_across_deep_sleep(self):
        with MockTrmnl(tls=True) as mock:
            mock.device_host = "trmnl.app"
            with onboard(mock, "--dns", "trmnl.app=10.0.2.2") as s:
                # /api/setup uses a plain client; /api/log's resumable client starts the session
                # the later requests resume
                self.assertEqual([(r.path, r.tls_resumed) for r in mock.requests],
                                 [("/api/setup", False), ("/images/default.bmp", False), ("/api/log", False),
                                  ("/api/display", True), ("/images/default.bmp", True)])
                self.assertEqual({r.tls_resumed for r in refresh(s, mock)}, {True})
                # a power cycle loses RTC memory, and with it the session: full handshake
                first = refresh(s, mock, wake=s.power_cycle)[0]
                self.assertEqual((first.path, first.tls_resumed), ("/api/display", False))
                self.assertEqual({r.tls_resumed for r in refresh(s, mock)}, {True})


if __name__ == "__main__":
    unittest.main()

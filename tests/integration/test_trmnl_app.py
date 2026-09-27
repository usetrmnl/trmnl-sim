"""Against the real trmnl.app API (opt-in: TRMNL_SIM_NETWORK=1)."""

import unittest

from support import GOLDEN, NETWORK, sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)


@unittest.skipUnless(NETWORK, "set TRMNL_SIM_NETWORK=1 to talk to trmnl.app")
class TrmnlApp(unittest.TestCase):
    def test_unregistered_device_message(self):
        with sim(erase=True) as s:
            s.wait(portal=True, timeout_s=90)
            refreshes = s.status()["display_refreshes"]
            s.portal_connect("TRMNL-Sim", "pw")
            s.wait(wifi_connected=True, timeout_s=60)
            s.wait(min_refreshes=refreshes + 1, display_idle=True, settle_ms=500, timeout_s=120)
            s.assert_screen(GOLDEN / "not_registered_text.png", region=(0, 320, 800, 50))
            s.wait(state="deep_sleep", timeout_s=120)


if __name__ == "__main__":
    unittest.main()

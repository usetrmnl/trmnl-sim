"""The captive portal of the device under test in setup mode: what phones and laptops probe, the
settings page, the advanced join options (static IP, WPA2 Enterprise, NTP server,
hostname) and several saved networks."""

import http.client
import json
import time
import unittest
from urllib.parse import urlparse

from support import MockTrmnl, ProvisionedDevice, close_fixtures, fixture, needs, sim

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

PARALLEL_BY_CLASS = True  # run.py gives each class its own worker

dev = fixture(ProvisionedDevice)


def tearDownModule():
    close_fixtures()


def raw_get(s, path: str, timeout: float = 30) -> tuple[int, dict, bytes]:
    """GET from the portal without following redirects: (status, headers, body)."""
    url = urlparse(s.portal_url())
    conn = http.client.HTTPConnection(url.hostname, url.port, timeout=timeout)
    try:
        conn.request("GET", path)
        r = conn.getresponse()
        return r.status, {k.lower(): v for k, v in r.getheaders()}, r.read()
    finally:
        conn.close()


def fresh(**kw):
    s = sim(erase=True, extra_args=("--offline",), **kw)
    try:
        s.wait(portal=True, timeout_s=90)
    except BaseException:
        s.close()
        raise
    return s


class Probes(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.sim = fresh()

    @classmethod
    def tearDownClass(cls):
        cls.sim.close()

    def test_captive_portal_checks_are_redirected_to_the_setup_page(self):
        for path in ("/generate_204", "/redirect", "/hotspot-detect.html", "/canonical.html", "/ncsi.txt",
                     "/no/such/page"):
            with self.subTest(path):
                code, headers, _ = raw_get(self.sim, path)
                self.assertEqual(code, 302)
                self.assertIn("location", headers)

    def test_windows_and_firefox_checks(self):
        code, headers, _ = raw_get(self.sim, "/connecttest.txt")
        self.assertEqual((code, headers.get("location")), (302, "http://logout.net"))
        self.assertEqual(raw_get(self.sim, "/wpad.dat")[0], 404)
        self.assertEqual(raw_get(self.sim, "/success.txt")[0], 200)
        self.assertEqual(raw_get(self.sim, "/favicon.ico")[0], 404)

    def test_setup_page_is_served_gzipped(self):
        code, headers, body = raw_get(self.sim, "/")
        self.assertEqual(code, 200)
        self.assertEqual(headers.get("content-encoding"), "gzip")
        self.assertGreater(len(body), 1000)
        code, headers, _ = raw_get(self.sim, "/advanced")
        self.assertEqual((code, headers.get("location")), (302, "/#advanced"))

    def test_device_settings_report_the_api_url(self):
        code, _, body = raw_get(self.sim, "/device-settings")
        self.assertEqual(code, 200)
        self.assertIn("api_url", json.loads(body))

    def test_sensor_self_test(self):
        # two averages of 1000 chip temperature readings, 7 s apart: over 9 s of device time,
        # more on the clock where the simulation runs slower than real time
        code, _, body = raw_get(self.sim, "/run-test", timeout=120)
        self.assertEqual(code, 200)
        json.loads(body)

    def test_forced_rescan(self):
        code, _, body = raw_get(self.sim, "/scan?force=1")
        self.assertIn(code, (200, 202))
        deadline = time.time() + 60
        while True:
            code, _, body = raw_get(self.sim, "/scan")
            if code != 202 or time.time() > deadline:
                break
            time.sleep(0.5)  # 202: still scanning (a 200 starts the next scan)
        self.assertEqual(code, 200)
        self.assertIn("TRMNL-Sim", [n["name"] for n in json.loads(body)["networks"]])


class ScanList(unittest.TestCase):
    def test_access_points_are_merged_by_ssid(self):
        nets = [{"ssid": "TRMNL-Sim", "rssi": -75}, {"ssid": "TRMNL-Sim", "rssi": -45, "channel": 11},
                {"ssid": "Cafe", "open": True, "rssi": -60}, {"ssid": "TRMNL", "rssi": -30}]
        with fresh(networks=nets) as s:
            scan = {n["name"]: n for n in s.portal_scan()["networks"]}
            self.assertEqual(scan["TRMNL-Sim"]["rssi"], "-45")  # the strongest of the two
            self.assertTrue(scan["Cafe"]["open"])
            self.assertNotIn("TRMNL", scan)  # another device's setup network


class JoinOptions(unittest.TestCase):
    def join(self, s, mock, **fields):
        body = {"ssid": "TRMNL-Sim", "pswd": "password", "server": mock.device_url, **fields}
        code, data = s.portal_request("/connect", body)
        self.assertEqual(code, 200, data)

    def test_static_ip(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, useStaticIP=True, staticIP="192.168.4.50", gateway="192.168.4.1",
                      subnet="255.255.255.0", dns1="1.1.1.1", dns2="8.8.8.8")
            s.wait_for_console(r"Static IP configured", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=180)

    def test_static_ip_with_defaults(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, useStaticIP=True, staticIP="192.168.4.50")
            s.wait_for_console(r"Static IP configured", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=180)

    def test_invalid_static_ip_falls_back_to_dhcp(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, useStaticIP=True, staticIP="not-an-ip")
            s.wait_for_console(r"Invalid static IP address", timeout_s=90)
            mock.wait_for_request("/api/setup", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=300)

    def test_wpa2_enterprise(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, isEnterprise=True, identity="alice@example.com", username="alice")
            s.wait_for_console(r"WPA2 Enterprise", timeout_s=90)
            mock.wait_for_request("/api/setup", timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=180)

    def test_wpa2_enterprise_identity_only(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, isEnterprise=True, identity="alice@example.com", pswd="")
            s.wait_for_console(r"WPA2 Enterprise", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=300)

    def test_wpa2_enterprise_without_identity_fails(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, isEnterprise=True)
            s.wait_for_console(r"requires an identity", timeout_s=90)
            s.wait(state="deep_sleep", timeout_s=300)

    def test_ntp_server_and_hostname_are_saved(self):
        with MockTrmnl() as mock, fresh() as s:
            self.join(s, mock, ntpServer1="time.example.com", hostname="kitchen-trmnl")
            s.wait_for_console(r"Saved NTP server: time.example.com", timeout_s=60)
            s.wait_for_console(r"Saved hostname: kitchen-trmnl", timeout_s=60)
            s.wait(state="deep_sleep", timeout_s=180)


class SavedNetworks(unittest.TestCase):
    def setUp(self):
        m = dev().mock
        m.requests.clear()
        m.display_queue.clear()
        m.display = {"image": "default", "refresh_rate": 300}

    def add_network(self, s, ssid: str, password: str):
        """Join another network through the add_wifi special function (which, unlike a
        long press, keeps the saved networks)."""
        m = dev().mock
        m.display = {"image": "default", "refresh_rate": 300, "special_function": "add_wifi"}
        n = len(m.requests)
        s.wake()
        m.wait_for_request("/api/display", after=n, timeout_s=120)
        s.wait(state="deep_sleep", timeout_s=120, settle_ms=200)
        m.display = {"image": "default", "refresh_rate": 300, "action": "add_wifi"}
        s.press(1500)
        s.wait(portal=True, timeout_s=120)
        return s

    @needs("button")  # add_network
    def test_second_network_is_used_when_the_first_is_gone(self):
        nets = [{"ssid": "TRMNL-Sim"}, {"ssid": "Office", "password": "office-pass", "rssi": -60}]
        with dev().boot_asleep(networks=nets) as s:
            s.wait(state="deep_sleep", timeout_s=90)
            self.add_network(s, "Office", "office-pass")
            s.portal_connect("Office", "office-pass", server=dev().mock.device_url)
            n = len(dev().mock.requests)
            dev().mock.wait_for_request("/api/display", after=n, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120, settle_ms=200)
            s.set_networks([{"ssid": "TRMNL-Sim"}])  # the office (last used) is out of range
            n, c = len(dev().mock.requests), s.status()["console_total"]
            s.wake()
            s.wait(console=r"Trying to connect to saved network TRMNL-Sim", since=c, timeout_s=240)
            dev().mock.wait_for_request("/api/display", after=n, timeout_s=240)
            s.wait(state="deep_sleep", timeout_s=120)

    @needs("button")
    def test_joining_a_saved_network_again_is_not_saved_twice(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            self.add_network(s, "TRMNL-Sim", "password")
            c = s.status()["console_total"]
            s.portal_connect("TRMNL-Sim", "password", server=dev().mock.device_url)
            s.wait(console=r"Duplicate regular network found", since=c, timeout_s=120)
            s.wait(state="deep_sleep", timeout_s=120)

    @needs("button")
    def test_portal_lists_saved_networks_out_of_range(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            self.add_network(s, "TRMNL-Sim", "password")
            s.set_networks([{"ssid": "Somewhere Else", "rssi": -70}])
            s.portal_request("/scan?force=1")
            deadline = time.time() + 60
            while time.time() < deadline:
                code, body = s.portal_request("/scan")
                if code == 200:
                    break
                time.sleep(0.5)
            saved = {n["name"]: n for n in json.loads(body)["networks"] if n["saved"]}
            self.assertIn("TRMNL-Sim", saved)

    def test_wifi_failures_on_timer_wakes_end_with_the_wifi_error(self):
        with dev().boot_asleep() as s:
            s.wait(state="deep_sleep", timeout_s=90)
            screen = s.screenshot()
            s.set_wifi(False)
            for _ in range(14):
                s.wake()
                s.wait(state="deep_sleep", timeout_s=240, settle_ms=200)
                if not s.compare_screen(screen, tolerance=0, max_ratio=0)["match"]:
                    break
            else:
                self.fail("the WiFi error was never shown")


if __name__ == "__main__":
    unittest.main()

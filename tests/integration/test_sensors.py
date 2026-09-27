"""Environment sensors on the TRMNL OG's I2C header (the simulator's --sensor): the
readings go to the server in the SENSORS header of /api/display."""

import unittest

from support import ProvisionedDevice, close_fixtures, fixture, needs

from devices import ANY

ENV = ANY  # general tests: they run on the device under test (see devices.py)

dev = fixture(ProvisionedDevice)


def tearDownModule():
    close_fixtures()


class Sensors(unittest.TestCase):
    def setUp(self):
        dev().mock.requests.clear()
        dev().mock.display = {"image": "default", "refresh_rate": 300}

    def sensor_header(self, *sensors) -> str:
        args = tuple(a for s in sensors for a in ("--sensor", s))
        with dev().boot_asleep(extra_args=args) as s:
            s.wait(state="deep_sleep", timeout_s=15)
            n = len(dev().mock.requests)
            s.wake()
            req = dev().mock.wait_for_request("/api/display", after=n, timeout_s=15)
            s.wait(state="deep_sleep", timeout_s=15)
            return req.headers.get("SENSORS", "")

    @needs("sensors")
    def test_scd41_reports_co2_temperature_and_humidity(self):
        header = self.sensor_header("scd41")
        self.assertIn("model=SCD41;kind=carbon_dioxide;value=812;unit=parts_per_million", header)
        self.assertIn("model=SCD41;kind=temperature;value=22.4", header)
        self.assertIn("model=SCD41;kind=humidity;value=44;unit=percent", header)

    @needs("sensors")
    def test_aht20_reports_temperature_and_humidity(self):
        header = self.sensor_header("aht20")
        self.assertIn("make=ASAIR;model=AHT20;kind=temperature;value=22.4", header)
        self.assertIn("model=AHT20;kind=humidity;value=44;unit=percent", header)

    @needs("sensors")
    def test_both_sensors(self):
        header = self.sensor_header("scd41", "aht20")
        self.assertIn("model=SCD41;kind=carbon_dioxide", header)
        self.assertIn("model=AHT20;kind=temperature", header)

    def test_no_sensor_no_header(self):
        self.assertEqual(self.sensor_header(), "")


if __name__ == "__main__":
    unittest.main()

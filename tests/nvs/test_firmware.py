#!/usr/bin/env python3
"""Exercise preference edits with unmodified firmware and a COPY of an onboarded flash.

The simulator uses offline networking; this never writes the supplied flash file.
"""
import argparse
import hashlib
import json
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def run(binary, firmware, seed):
    with tempfile.TemporaryDirectory(prefix='trmnl-preferences-') as directory:
        flash = Path(directory) / 'flash.bin'
        shutil.copyfile(seed, flash)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'

        def call(path, body=None, method=None, expected=200):
            request = urllib.request.Request(base + path, method=method,
                data=None if body is None else json.dumps(body).encode(),
                headers={'Content-Type': 'application/json'})
            try:
                response = urllib.request.urlopen(request, timeout=10)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                result = json.load(response)
                assert response.code == expected, (path, response.code, result.get('error'))
                return result

        def wait(predicate, message, seconds=60):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                assert process.poll() is None, 'simulator exited; see console log'
                try:
                    result = predicate()
                    if result:
                        return result
                except urllib.error.URLError:
                    pass
                time.sleep(0.1)
            raise AssertionError(message)

        def entries():
            snapshot = call('/preferences')
            assert not snapshot['warnings'], snapshot['warnings']
            return {(e['partition'], e['namespace'], e['key']): (e['type'], e['value']) for e in snapshot['entries']}

        def edit(namespace, key, kind=None, value=None, expected=200):
            body = {'partition': 'nvs', 'namespace': namespace, 'key': key}
            if kind is not None:
                body.update(type=kind, value=value)
            return call('/preferences', body, 'DELETE' if kind is None else 'PUT', expected)

        args = [binary, firmware, '--headless', '--turbo', '--offline', '--flash', str(flash),
                '--portal-port', '0', '--control', f'127.0.0.1:{port}']
        with open(Path(directory) / 'console.log', 'wb') as log:
            process = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
            try:
                wait(lambda: call('/status')['state'] == 'deep_sleep', 'seed firmware did not enter deep sleep')
                call('/pause', {'on': True})  # Freeze the sleep timer while exercising edits.
                assert call('/preferences')['editable']
                before = entries()
                edit('data', 'friendly_id', 'string', 'PREFS123')
                edit('sim_test', 'counter', 'u64', '18446744073709551615')
                edit('sim_test', 'large_blob', 'blob', 'a5' * 5000)
                current = entries()
                assert current['nvs', 'data', 'friendly_id'] == ('string', 'PREFS123')
                assert current['nvs', 'sim_test', 'counter'] == ('u64', '18446744073709551615')
                assert current['nvs', 'sim_test', 'large_blob'] == ('blob', ' '.join(['a5'] * 5000))
                for key, value in before.items():
                    if key != ('nvs', 'data', 'friendly_id'):
                        assert current[key] == value, ('unrelated preference changed', key)
                digest = hashlib.sha256(flash.read_bytes()).digest()
                edit('sim_test', 'counter', 'u8', '256', expected=409)
                assert hashlib.sha256(flash.read_bytes()).digest() == digest
                edit('sim_test', 'counter')
                assert ('nvs', 'sim_test', 'counter') not in entries()
                assert call('/preferences')['editable']  # Writes did not wake the device.
                print('PASS: read, set, create, delete, validation, unrelated values, multi-page blob', flush=True)

                since = call('/console')['total']
                call('/wake', {})
                call('/pause', {'on': False})
                wait(lambda: any('PREFS123' in line['text'] for line in call(f'/console?since={since}')['lines']),
                     'firmware did not read the edited friendly ID on wake')
                wait(lambda: call('/status')['state'] == 'deep_sleep', 'firmware did not return to deep sleep')
                assert entries()['nvs', 'sim_test', 'large_blob'] == ('blob', ' '.join(['a5'] * 5000))
                print('PASS: unchanged firmware read the edit on wake and retained the multi-page blob', flush=True)

                call('/pause', {'on': True})
                call('/reset', {})  # Paused with a powered CPU, rather than paused deep sleep.
                assert not call('/preferences')['editable']
                digest = hashlib.sha256(flash.read_bytes()).digest()
                edit('data', 'friendly_id', 'string', 'REJECTED', expected=409)
                edit('data', 'friendly_id', expected=409)
                assert hashlib.sha256(flash.read_bytes()).digest() == digest
                call('/quit', {})
                process.wait(timeout=10)
                process = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
                wait(lambda: call('/status')['state'] == 'deep_sleep', 'restarted firmware did not sleep')
                assert entries()['nvs', 'data', 'friendly_id'] == ('string', 'PREFS123')
                assert ('nvs', 'sim_test', 'counter') not in entries()
                assert entries()['nvs', 'sim_test', 'large_blob'] == ('blob', ' '.join(['a5'] * 5000))
                print('PASS: paused-CPU writes rejected; changes and deletion survived restart', flush=True)
            finally:
                if process.poll() is None:
                    try:
                        call('/quit', {})
                        process.wait(timeout=5)
                    except Exception:
                        process.kill()
                        process.wait()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('firmware', help='existing firmware build directory')
    parser.add_argument('flash', help='onboarded flash to COPY (must boot into deep sleep)')
    parser.add_argument('--binary', default='./target/release/trmnl-sim')
    args = parser.parse_args()
    run(args.binary, args.firmware, args.flash)

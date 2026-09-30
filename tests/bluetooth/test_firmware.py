#!/usr/bin/env python3
"""Integration check against an unmodified BLE-enabled TRMNL firmware build."""
import argparse
import contextlib
import json
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path


class AttError(Exception):
    pass


def request(base, path, body=None):
    req = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(),
                                 headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=15) as response:
        return json.load(response)


def run(binary, firmware, qr_decoder=None, provision=False):
    with tempfile.TemporaryDirectory(prefix='trmnl-ble-test-') as directory, contextlib.ExitStack() as fixtures:
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        network_args = ['--offline']
        if provision:
            from local_tls import LocalTLS
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                mock_port = sock.getsockname()[1]
            tls = fixtures.enter_context(LocalTLS(directory, mock_port))
            network_args += ['--mock-server=' + str(mock_port), '--dns', 'trmnl.eliz.live=10.0.2.2',
                             '--host-port', f'443={tls.port}']
        with open(Path(directory) / 'console.log', 'wb+') as log:
            process = subprocess.Popen([binary, firmware, '--headless', '--turbo', *network_args,
                '--flash', str(Path(directory) / 'flash.bin'), '--erase', '--portal-port', '0',
                '--control', f'127.0.0.1:{port}'], stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 60
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        log.seek(0)
                        raise AssertionError('Simulator exited: ' + log.read().decode(errors='replace')[-3000:])
                    try:
                        status = request(base, '/status')['bluetooth']
                        if status['advertising']:
                            break
                    except (OSError, KeyError):
                        pass
                    time.sleep(0.05)
                else:
                    raise AssertionError('Firmware did not advertise within 60 seconds')
                assert status['active'] == 'mock' and status['initialized']
                assert 'requested' not in status and 'fallback_reason' not in status
                link = request(base, '/bluetooth/connect', {})['connection']
                def exchange(data):
                    return bytes(request(base, '/bluetooth/att', {'connection': link, 'data': list(data)})['data'])
                # ATT Read By Group Type for primary services, then characteristic discovery.
                services = exchange(bytes.fromhex('100100ffff0028'))
                assert services[0] == 0x11, services.hex()
                mtu = exchange(bytes.fromhex('020502'))
                assert mtu[0] == 3, mtu.hex()
                att_mtu = min(517, int.from_bytes(mtu[1:], 'little'))
                # Find all characteristic declarations; use UUIDs, never firmware handle constants.
                start, characteristics = 1, {}
                while start <= 0xffff:
                    response = exchange(bytes([8, start & 255, start >> 8, 255, 255, 3, 40]))
                    if response[0] == 1:
                        assert response[4] == 10, response.hex()
                        break
                    assert response[0] == 9 and response[1] in (7, 21), response.hex()
                    size = response[1]
                    for i in range(2, len(response), size):
                        row = response[i:i + size]
                        start = int.from_bytes(row[:2], 'little') + 1
                        if size == 21:
                            characteristics[row[5:][::-1].hex()] = int.from_bytes(row[3:5], 'little')
                handle = characteristics['7c3e00014e914f829a6320be63ec51d4']
                address = handle.to_bytes(2, 'little')
                assert exchange(b'\x12' + address + b'ESP') == b'\x13'
                version = exchange(b'\x0a' + address)
                assert version[0] == 11
                info = json.loads(version[1:])
                assert info['prov']['sec_ver'] == 1 and info['trmnl']['ver'] == 1, info
                if qr_decoder:
                    # Decode the firmware-rendered QR; never read internal pairing state.
                    screenshot = Path(directory) / 'screen.png'
                    deadline = time.monotonic() + 20
                    while time.monotonic() < deadline:
                        screenshot.write_bytes(urllib.request.urlopen(base + '/screenshot', timeout=5).read())
                        decoded = subprocess.run([qr_decoder, str(screenshot)], capture_output=True, text=True)
                        if decoded.returncode == 0:
                            break
                        time.sleep(0.2)
                    else:
                        raise AssertionError('Could not decode firmware pairing QR')
                    parts = decoded.stdout.strip().split('#BLE:')[1].split(':')
                    assert parts[0] == '1'
                    session, proof = str(uuid.UUID(parts[2])), parts[3]
                    from security1 import authenticate, status as secured_status
                    def endpoint(uuid):
                        handle = characteristics[uuid].to_bytes(2, 'little')
                        def call(data):
                            if len(data) <= att_mtu - 3:
                                written = exchange(b'\x12' + handle + data)
                                if written[0] == 1:
                                    raise AttError(written.hex())
                                assert written == b'\x13'
                            else:
                                for offset in range(0, len(data), att_mtu - 5):
                                    fragment = handle + offset.to_bytes(2, 'little') + data[offset:offset + att_mtu - 5]
                                    assert exchange(b'\x16' + fragment) == b'\x17' + fragment
                                executed = exchange(b'\x18\x01')
                                assert executed == b'\x19', executed.hex()
                            response = exchange(b'\x0a' + handle)
                            assert response[0] == 11, response.hex()
                            output = response[1:]
                            while len(response) == att_mtu:
                                response = exchange(b'\x0c' + handle + len(output).to_bytes(2, 'little'))
                                assert response[0] == 13, response.hex()
                                output += response[1:]
                            return output
                        return call
                    security = endpoint('7c3e00024e914f829a6320be63ec51d4')
                    stream = authenticate(security, proof)
                    secured_status(endpoint('7c3e00034e914f829a6320be63ec51d4'), stream, session)
                    secured_status(endpoint('7c3e00034e914f829a6320be63ec51d4'), stream, session, padding=300)
                    # IDF NimBLE limits each attribute to 512 bytes. Rejection must
                    # remain a guest ATT error and must not consume the cipher stream.
                    control_handle = characteristics['7c3e00034e914f829a6320be63ec51d4'].to_bytes(2, 'little')
                    oversized = b'z' * 513
                    for offset in range(0, len(oversized), att_mtu - 5):
                        fragment = control_handle + offset.to_bytes(2, 'little') + oversized[offset:offset + att_mtu - 5]
                        assert exchange(b'\x16' + fragment) == b'\x17' + fragment
                    assert exchange(b'\x18\x01') == b'\x01\x18' + control_handle + b'\x0d'
                    secured_status(endpoint('7c3e00034e914f829a6320be63ec51d4'), stream, session)
                    print('PASS: firmware QR, Security 1, encrypted status, long write and oversize rejection')
                    if provision:
                        control = endpoint('7c3e00034e914f829a6320be63ec51d4')
                        def command(op, attempt=0, **params):
                            message = json.dumps(dict(v=1, sid=session, op=op, attempt=attempt, **params),
                                                 separators=(',', ':')).encode()
                            response = json.loads(stream.update(control(stream.update(message))))
                            assert response['sid'] == session and response['v'] == 1
                            return response
                        networks = command('networks', offset=0, limit=3)
                        assert 'error' not in networks and isinstance(networks['networks'], list), networks
                        invalid = command('configure', attempt=1, ssid='TRMNL-Sim', password='short')
                        assert invalid['error'] == 'invalid_credentials' and invalid['attempt'] == 0, invalid
                        configured = command('configure', attempt=1, ssid='TRMNL-Sim', password='test-password')
                        assert 'error' not in configured and configured['attempt'] == 1, configured
                        request(base, '/wifi', {'portal_client': False})
                        deadline = time.monotonic() + 45
                        while time.monotonic() < deadline:
                            response = command('status', attempt=1)
                            assert 'error' not in response, response
                            if response['state'] == 'code_ready':
                                break
                            time.sleep(0.2)
                        else:
                            raise AssertionError('Provisioning did not produce a setup code')
                        assert response['friendly_id'] == 'SIMTST', response
                        ack = command('ack_code', attempt=1)
                        assert ack['state'] == 'handed_off' and 'error' not in ack, ack
                        print('PASS: encrypted Wi-Fi configuration, local TLS setup, setup code and acknowledgment')
                        return

                    request(base, '/bluetooth/disconnect', {'connection': link})
                    deadline = time.monotonic() + 5
                    while not request(base, '/status')['bluetooth']['advertising']:
                        assert time.monotonic() < deadline, 'Firmware failed to resume advertising'
                        time.sleep(0.05)
                    old_link = link
                    link = request(base, '/bluetooth/connect', {})['connection']
                    assert old_link != link
                    mtu = exchange(bytes.fromhex('020502'))
                    assert mtu[0] == 3
                    stream = authenticate(security, proof)
                    secured_status(endpoint('7c3e00034e914f829a6320be63ec51d4'), stream, session)
                    print('PASS: disconnect/reconnect uses a fresh guest security session')
                request(base, '/bluetooth/disconnect', {'connection': link})
                if qr_decoder:
                    deadline = time.monotonic() + 5
                    while not request(base, '/status')['bluetooth']['advertising']:
                        assert time.monotonic() < deadline
                        time.sleep(0.05)
                    link = request(base, '/bluetooth/connect', {})['connection']
                    assert exchange(bytes.fromhex('020502'))[0] == 3
                    wrong_proof = ('1' if proof[0] == '0' else '0') + proof[1:]
                    try:
                        authenticate(security, wrong_proof)
                    except AttError:
                        pass
                    except urllib.error.HTTPError as error:
                        failure = json.load(error)
                        assert error.code == 409 and 'timed out' not in failure['error'], failure
                    else:
                        raise AssertionError('Firmware accepted wrong proof')
                    print('PASS: firmware rejects wrong proof')
                request(base, '/reset', {})
                try:
                    exchange(bytes.fromhex('0a0100'))
                except urllib.error.HTTPError as error:
                    assert error.code == 409
                else:
                    raise AssertionError('Reset accepted stale connection token')
                print('PASS: real NimBLE discovery, MTU, proto-ver, configuration and reset invalidation')
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                log.seek(0)
                Path('/tmp/trmnl-ble-firmware-test.log').write_bytes(log.read())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('firmware')
    parser.add_argument('--binary', default='target/release/trmnl-sim')
    parser.add_argument('--qr-decoder', help='Executable that decodes the firmware screenshot; enables Security 1 checks (requires cryptography)')
    parser.add_argument('--provision', action='store_true', help='Also verify Wi-Fi/setup-code handoff against an isolated local TLS server (requires --qr-decoder)')
    args = parser.parse_args()
    if args.provision and not args.qr_decoder:
        parser.error('--provision requires --qr-decoder')
    run(args.binary, args.firmware, args.qr_decoder)

    if args.provision:
        run(args.binary, args.firmware, args.qr_decoder, provision=True)

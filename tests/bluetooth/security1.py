"""Independent Security 1 test central (ESP-IDF session.proto/sec1.proto)."""
import hashlib
import json
from cryptography.hazmat.primitives.asymmetric import x25519
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat


def varint(value):
    output = bytearray()
    while value > 127:
        output.append((value & 127) | 128)
        value >>= 7
    return bytes(output + bytes([value]))


def scalar(field, value):
    return varint(field << 3) + varint(value)


def blob(field, value):
    return varint((field << 3) | 2) + varint(len(value)) + value


def fields(data):
    result, index = {}, 0
    def read():
        nonlocal index
        value, shift = 0, 0
        while index < len(data) and shift < 64:
            byte = data[index]
            index += 1
            value |= (byte & 127) << shift
            if byte < 128:
                return value
            shift += 7
        raise AssertionError('Invalid protobuf varint')
    while index < len(data):
        tag = read()
        if tag & 7 == 0:
            value = read()
        else:
            assert tag & 7 == 2
            size = read()
            assert index + size <= len(data)
            value = data[index:index + size]
            index += size
        assert tag >> 3 not in result
        result[tag >> 3] = value
    return result


def authenticate(endpoint, proof):
    private = x25519.X25519PrivateKey.generate()
    public = private.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    response = fields(endpoint(scalar(2, 1) + blob(11, blob(20, blob(1, public)))))
    assert response[2] == 1
    payload = fields(response[11])
    assert payload[1] == 1
    hello = fields(payload[21])
    assert hello.get(1, 0) == 0
    peer, iv = hello[2], hello[3]
    assert len(peer) == 32 and len(iv) == 16
    shared = private.exchange(x25519.X25519PublicKey.from_public_bytes(peer))
    digest = hashlib.sha256(proof.encode()).digest()
    key = bytes(a ^ b for a, b in zip(shared, digest))
    stream = Cipher(algorithms.AES(key), modes.CTR(iv)).encryptor()
    command = scalar(2, 1) + blob(11, scalar(1, 2) + blob(22, blob(2, stream.update(peer))))
    response = fields(endpoint(command))
    assert response[2] == 1
    payload = fields(response[11])
    assert payload[1] == 3
    verifier = fields(payload[23])
    assert verifier.get(1, 0) == 0
    assert stream.update(verifier[3]) == public, 'Device Security 1 verifier mismatch'
    return stream


def status(endpoint, stream, session, padding=0):
    message = json.dumps({'v': 1, 'sid': session, 'op': 'status', 'attempt': 0}, separators=(',', ':')).encode()
    message += b' ' * padding
    response = json.loads(stream.update(endpoint(stream.update(message))))
    assert response['v'] == 1 and response['sid'] == session
    assert 'error' not in response, response.get('error')
    return response

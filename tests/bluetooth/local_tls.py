"""Loopback-only TLS proxy for the unmodified firmware's fixed setup URL.

The firmware's normal HTTP wrapper uses setInsecure(), so this short-lived test
certificate needs no trust-store changes. The simulator redirects DNS and port
443 only inside its isolated virtual network; no host DNS or server is changed.
"""
import datetime
import http.server
import ssl
import threading
import urllib.request
from pathlib import Path
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID


class LocalTLS:
    def __init__(self, directory, upstream):
        key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        subject = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, 'trmnl.eliz.live')])
        now = datetime.datetime.now(datetime.timezone.utc)
        cert = (x509.CertificateBuilder().subject_name(subject).issuer_name(subject).public_key(key.public_key())
                .serial_number(x509.random_serial_number()).not_valid_before(now - datetime.timedelta(days=1))
                .not_valid_after(now + datetime.timedelta(days=1)).sign(key, hashes.SHA256()))
        cert_path, key_path = Path(directory) / 'cert.pem', Path(directory) / 'key.pem'
        cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
        key_path.touch(mode=0o600)
        key_path.write_bytes(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                with urllib.request.urlopen(f'http://127.0.0.1:{upstream}' + self.path, timeout=10) as r:
                    body = r.read()
                    self.send_response(r.status)
                    self.send_header('Content-Type', r.headers.get('Content-Type', 'application/octet-stream'))
                    self.send_header('Content-Length', str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
            def log_message(self, *args):
                pass

        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert_path, key_path)

        class Server(http.server.ThreadingHTTPServer):
            def get_request(self):
                sock, address = super().get_request()
                sock.settimeout(10)
                try:
                    return context.wrap_socket(sock, server_side=True), address
                except Exception:
                    sock.close()
                    raise

        self.server = Server(('127.0.0.1', 0), Handler)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

#!/usr/bin/env python3
"""Fake upstream HTTP proxy that REQUIRES Basic auth. For end-to-end testing only.

Usage: fake_upstream_proxy.py <port> <user> <password>
Supports CONNECT tunneling and absolute-URI GET forwarding.
"""
import base64
import socket
import socketserver
import sys
import threading

USER, PASSWORD = "", ""


def pipe(a: socket.socket, b: socket.socket) -> None:
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


class Handler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.connection.recv(4096)
            if not chunk:
                return
            head += chunk
        text = head.decode("latin1")
        lines = text.split("\r\n")
        request_line = lines[0]
        expected = base64.b64encode(f"{USER}:{PASSWORD}".encode()).decode()
        got = next(
            (l.split(":", 1)[1].strip() for l in lines if l.lower().startswith("proxy-authorization:")),
            "",
        )
        if got != f"Basic {expected}":
            print(f"[upstream] 407 for {request_line!r}", flush=True)
            self.connection.sendall(
                b'HTTP/1.1 407 Proxy Authentication Required\r\n'
                b'Proxy-Authenticate: Basic realm="test"\r\n'
                b"Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            return
        print(f"[upstream] AUTH OK {request_line}", flush=True)
        method, target = request_line.split(" ")[0], request_line.split(" ")[1]
        if method.upper() == "CONNECT":
            host, _, port = target.rpartition(":")
            self.tunnel(host, int(port), b"")
        else:
            rest = text.split("\r\n\r\n", 1)[1].encode("latin1")
            from urllib.parse import urlsplit

            u = urlsplit(target)
            path = u.path or "/"
            if u.query:
                path += "?" + u.query
            rebuilt = f"{method} {path} HTTP/1.1\r\nHost: {u.netloc}\r\nConnection: close\r\n\r\n"
            self.tunnel(u.hostname, u.port or 80, rebuilt.encode() + rest, greet=False)

    def tunnel(self, host: str, port: int, initial: bytes, greet: bool = True) -> None:
        try:
            remote = socket.create_connection((host, port), timeout=10)
        except OSError as e:
            self.connection.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            print(f"[upstream] 502 {host}:{port} {e}", flush=True)
            return
        if greet:
            self.connection.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
        if initial:
            remote.sendall(initial)
        t = threading.Thread(target=pipe, args=(self.connection, remote), daemon=True)
        t.start()
        pipe(remote, self.connection)
        t.join(timeout=5)


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    port, USER, PASSWORD = int(sys.argv[1]), sys.argv[2], sys.argv[3]
    with Server(("127.0.0.1", port), Handler) as srv:
        print(f"[upstream] listening on 127.0.0.1:{port}", flush=True)
        srv.serve_forever()

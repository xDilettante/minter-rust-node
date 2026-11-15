"""Check that the local test link actually cuts and restores TCP traffic."""

import socket
import socketserver
import threading
import unittest
from unittest.mock import patch

from tcp_link import TcpLink


class Echo(socketserver.BaseRequestHandler):
    def handle(self):
        while data := self.request.recv(1024):
            self.request.sendall(data)


class EchoServer(socketserver.ThreadingTCPServer):
    daemon_threads = True


class LinkTest(unittest.TestCase):
    def test_cut_cannot_be_missed_between_accept_and_registration(self):
        accepted, resume = threading.Event(), threading.Event()
        listener = socket.create_server(("127.0.0.1", 0))

        class DelayedAccept:
            def __getattr__(self, name):
                return getattr(listener, name)

            def accept(self):
                connection = listener.accept()
                accepted.set()
                if not resume.wait(timeout=3):
                    raise RuntimeError("test did not resume accept")
                return connection

        with EchoServer(("127.0.0.1", 0), Echo) as echo:
            worker = threading.Thread(target=echo.serve_forever, daemon=True)
            worker.start()
            with patch("tcp_link.socket.create_server", return_value=DelayedAccept()):
                link = TcpLink(echo.server_address[1])
            try:
                with socket.create_connection(("127.0.0.1", link.port), timeout=2) as connection:
                    connection.sendall(b"before-cut")
                    self.assertTrue(accepted.wait(timeout=2))
                    link.set_enabled(False)
                    link.set_enabled(True)
                    resume.set()
                    self.assertEqual(self.read_closed(connection), b"")
            finally:
                resume.set()
                link.close()
                echo.shutdown()
                worker.join(timeout=2)

    def test_cut_closes_a_pending_connect_and_it_cannot_survive_healing(self):
        connecting, resume = threading.Event(), threading.Event()
        real_connect = socket.create_connection

        def delayed_connect(*args, **kwargs):
            connecting.set()
            self.assertTrue(resume.wait(timeout=3))
            return real_connect(*args, **kwargs)

        with EchoServer(("127.0.0.1", 0), Echo) as echo:
            worker = threading.Thread(target=echo.serve_forever, daemon=True)
            worker.start()
            link = TcpLink(echo.server_address[1])
            try:
                with patch("tcp_link.socket.create_connection", side_effect=delayed_connect):
                    with real_connect(("127.0.0.1", link.port), timeout=2) as connection:
                        connection.sendall(b"before-cut")
                        self.assertTrue(connecting.wait(timeout=2))
                        link.set_enabled(False)
                        self.assertEqual(self.read_closed(connection), b"")
                        link.set_enabled(True)
                        resume.set()
            finally:
                resume.set()
                link.close()
                echo.shutdown()
                worker.join(timeout=2)

    def test_cuts_existing_and_new_connections_then_recovers(self):
        with EchoServer(("127.0.0.1", 0), Echo) as echo:
            worker = threading.Thread(target=echo.serve_forever, daemon=True)
            worker.start()
            link = TcpLink(echo.server_address[1])
            try:
                with socket.create_connection(("127.0.0.1", link.port), timeout=2) as connection:
                    connection.sendall(b"before")
                    self.assertEqual(connection.recv(6), b"before")
                    link.set_enabled(False)
                    self.assertEqual(self.read_closed(connection), b"")
                with socket.create_connection(("127.0.0.1", link.port), timeout=2) as connection:
                    connection.sendall(b"blocked")
                    self.assertEqual(self.read_closed(connection), b"")
                link.set_enabled(True)
                with socket.create_connection(("127.0.0.1", link.port), timeout=2) as connection:
                    connection.sendall(b"after")
                    self.assertEqual(connection.recv(5), b"after")
                self.assertGreaterEqual(link.stats()["bytes"], 22)
            finally:
                link.close()
                echo.shutdown()
                worker.join(timeout=2)

    @staticmethod
    def read_closed(connection):
        try:
            return connection.recv(1024)
        except (ConnectionResetError, ConnectionAbortedError):
            return b""


if __name__ == "__main__":
    unittest.main()

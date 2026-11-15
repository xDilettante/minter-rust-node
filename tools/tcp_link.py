"""A switchable loopback TCP link for local integration tests."""

import select
import socket
import threading


class TcpLink:
    def __init__(self, target_port):
        self.target = ("127.0.0.1", target_port)
        self.listener = socket.create_server(("127.0.0.1", 0))
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self.lock = threading.Lock()
        self.enabled = True
        self.closed = threading.Event()
        self.connections = set()
        self.pending = set()
        self.generation = 0
        self.workers = []
        self.accepted = self.bytes = 0
        self.acceptor = threading.Thread(target=self.accept, daemon=True)
        self.acceptor.start()

    def accept(self):
        while not self.closed.is_set():
            with self.lock:
                generation = self.generation
            try:
                left, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            left.settimeout(2)
            with self.lock:
                enabled = self.enabled and generation == self.generation
                if enabled and not self.closed.is_set():
                    self.pending.add(left)
                else:
                    enabled = False
            if not enabled:
                left.close()
                continue
            try:
                right = socket.create_connection(self.target, timeout=2)
            except OSError:
                with self.lock:
                    self.pending.discard(left)
                left.close()
                continue
            pair = (left, right)
            with self.lock:
                self.pending.discard(left)
                if not self.enabled or self.closed.is_set() or generation != self.generation:
                    left.close()
                    right.close()
                    continue
                self.connections.add(pair)
                self.accepted += 1
            worker = threading.Thread(target=self.forward, args=(pair,), daemon=True)
            self.workers.append(worker)
            worker.start()

    def forward(self, pair):
        try:
            while not self.closed.is_set():
                ready, _, _ = select.select(pair, [], [], 0.2)
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    with self.lock:
                        self.bytes += len(data)
                    target = pair[1] if source is pair[0] else pair[0]
                    target.sendall(data)
        except (OSError, ValueError):
            pass
        finally:
            self.disconnect(pair)
            with self.lock:
                self.connections.discard(pair)

    @staticmethod
    def disconnect(pair):
        for connection in pair:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()

    def set_enabled(self, enabled):
        with self.lock:
            self.enabled = enabled
            active = list(self.connections) if not enabled else []
            pending = list(self.pending) if not enabled else []
            if not enabled:
                self.generation += 1
        for pair in active:
            self.disconnect(pair)
        for connection in pending:
            self.disconnect((connection,))

    def stats(self):
        with self.lock:
            sessions = []
            for left, right in self.connections:
                try:
                    sessions.append({"sourcePort": left.getpeername()[1], "upstreamPort": right.getsockname()[1]})
                except OSError:
                    pass
            return {"enabled": self.enabled, "connections": len(self.connections),
                    "pending": len(self.pending),
                    "accepted": self.accepted, "bytes": self.bytes, "sessions": sessions}

    def close(self):
        self.closed.set()
        self.set_enabled(False)
        self.listener.close()
        self.acceptor.join(timeout=3)
        for worker in self.workers:
            worker.join(timeout=3)

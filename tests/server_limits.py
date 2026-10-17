"""Additional real-process checks; shared C++ integration suite stays unchanged."""
import concurrent.futures
import importlib.util
from pathlib import Path
import sys
import time

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location(
    'boncas_integration', Path(__file__).resolve().parent / 'integration.py')
integration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(integration)


def closed(stream):
    try:
        assert stream.recv(1) == b''
    except (ConnectionResetError, ConnectionAbortedError):
        pass


def connect_during_burst(port):
    # Windows may immediately reset a SYN when its small listen queue fills.
    # Retry briefly; a true server-side client limit still fails persistently.
    deadline = time.monotonic() + 2
    while True:
        try:
            return integration.connect(port)
        except ConnectionRefusedError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(.03)


def main():
    with integration.server() as (port, process):
        # Readiness probe has disconnected; allow its worker to be collected.
        time.sleep(.15)
        clients = []
        try:
            for _ in range(128):
                clients.append(connect_during_burst(port))
            # Every connection must be live, rather than merely in the backlog.
            with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
                replies = list(pool.map(lambda s: integration.exchange(s, integration.INIT), clients))
            assert all(len(reply) == 59 for reply in replies)
            with integration.connect(port) as extra:
                closed(extra)
            clients.pop().close()
            time.sleep(.2)
            with integration.connect(port) as recovered:
                assert len(integration.exchange(recovered, integration.INIT)) == 59
        finally:
            for stream in clients:
                stream.close()
        assert 'Client rejected:' in process.mock_logs()

    with integration.server() as (port, _):
        with integration.connect(port) as slow:
            integration.auth(slow, 51)
            started = time.monotonic()
            slow.sendall(b'\xff')
            # Bytes keep arriving within an ordinary per-read timeout.
            for _ in range(4):
                time.sleep(.18)
                slow.sendall(b'\x90')
            # The absolute authentication deadline still closes the frame.
            slow.settimeout(.8)
            closed(slow)
            assert time.monotonic() - started < 1.6
        integration.session(port, 52)
    print('Connection limit and absolute auth deadline OK')


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('usage: server_limits.py PATH_TO_BONCASRELAY')
    main()

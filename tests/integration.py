"""Real child process and TCP sockets; framing follows the compatible card protocol."""
import concurrent.futures
import contextlib
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

# Hosted Windows runners may have no console. Give child process groups one so
# CTRL_BREAK_EVENT exercises the actual server Ctrl+C/Break shutdown handler.
if os.name == 'nt':
    import ctypes
    kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)
    if not kernel32.GetConsoleCP():
        if not kernel32.AllocConsole():
            raise ctypes.WinError(ctypes.get_last_error())

EXE = str(Path(sys.argv[1]).resolve())
INIT = bytes.fromhex('90 30 00 00 00')
ECM = bytes.fromhex('90 34 00 01 00')


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def connect(port, host="127.0.0.1"):
    return socket.create_connection((host, port), 6)


def exact(s, n):
    b = bytearray()
    while len(b) < n:
        part = s.recv(n - len(b))
        if not part:
            raise EOFError('truncated reply')
        b.extend(part)
    return bytes(b)


def reply(s):
    return exact(s, exact(s, 1)[0])


def exchange(s, apdu):
    s.sendall(bytes([len(apdu)]) + apdu)
    return reply(s)


def stop(p):
    if p.poll() is None:
        if os.name == 'nt':
            p.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            p.send_signal(signal.SIGTERM)
    p.wait(timeout=12)
    assert p.returncode == 0, p.returncode


@contextlib.contextmanager
def server(*args, host="127.0.0.1", env=None):
    port = free_port()
    # The child writes through its own descriptor. Opening the named file for
    # reads avoids changing the shared output position while the child runs.
    with tempfile.TemporaryDirectory() as logdir:
        logpath = Path(logdir) / 'server.log'
        logs = logpath.open('wb')
        kwargs = {'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == 'nt' else {}
        p = subprocess.Popen([EXE, '--mock', '--listen', host, '--port', str(port), *args],
                             stdout=logs, stderr=logs,
                             env={**os.environ, **(env or {})}, **kwargs)
        p.mock_logs = lambda: logpath.read_text(encoding='utf-8', errors='replace')
        try:
            deadline = time.monotonic() + 8
            while True:
                assert p.poll() is None, 'server exited during startup'
                try:
                    with connect(port, host):
                        break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(.03)
            yield port, p
        except BaseException:
            print(p.mock_logs(), file=sys.stderr)
            raise
        finally:
            try:
                stop(p)
            finally:
                if p.poll() is None:
                    p.kill()
                    p.wait()
                logs.close()


def auth(s, token):
    result = exchange(s, bytes([0x90, 0xA0, 0, 1, token]))
    assert len(result) == 48 and result[0] == 0xA1


def session(port, token):
    with connect(port) as s:
        auth(s, token)
        time.sleep(.015)
        assert exchange(s, ECM) == bytes([0xB1, token, 0x90, 0])


def mock_counts(p):
    logs = p.mock_logs()
    return tuple(len(re.findall(r'^MOCK ' + kind + r' \d+$', logs, re.MULTILINE))
                 for kind in ('enumerate', 'probe'))


def burst(s, apdu, count, expected):
    s.sendall((bytes([len(apdu)]) + apdu) * count)
    for _ in range(count):
        assert reply(s) == expected


def test_detection_cache():
    env = {'BONCASRELAY_MOCK_DETECT_INTERVAL_MS': '500'}
    bcas_ecm = bytes.fromhex('90 34 00 00 00')
    reset = bytes.fromhex('90 FE 00 00 00')
    with server(env=env) as (port, p):
        with connect(port) as s:
            assert exchange(s, bcas_ecm) == b'\xb0\x90\0'
            initial = mock_counts(p)
            assert initial[0] == 1 and initial[1] > 0, initial
            burst(s, bcas_ecm, 50, b'\xb0\x90\0')
            time.sleep(.55)  # Aging alone must never cause probing.
            burst(s, bcas_ecm, 50, b'\xb0\x90\0')
            assert mock_counts(p) == initial
            assert exchange(s, reset) == b''
            assert exchange(s, bcas_ecm) == b'\xb0\x90\0'
            refreshed = mock_counts(p)
            assert refreshed == (initial[0] + 1, initial[1] + 2), refreshed
            # A second error does not bypass the minimum detection interval.
            assert exchange(s, reset) == b''
            burst(s, bcas_ecm, 20, b'\xb0\x90\0')
            assert mock_counts(p) == refreshed
            time.sleep(.55)
            assert exchange(s, bcas_ecm) == b'\xb0\x90\0'
            assert mock_counts(p) == (initial[0] + 2, initial[1] + 4)
            burst(s, bcas_ecm, 20, b'\xb0\x90\0')
            assert mock_counts(p) == (initial[0] + 2, initial[1] + 4)

    fixed = free_port()
    with server('--reader-port', f'C-CAS={fixed}', env=env) as (_, p):
        with connect(fixed) as s:
            assert exchange(s, INIT)[7] == 2
            assert mock_counts(p) == (1, 0)
            burst(s, bcas_ecm, 30, b'\xb2\x90\0')
            assert mock_counts(p) == (1, 0)
            assert exchange(s, reset) == b''
            assert exchange(s, INIT)[7] == 2
            assert mock_counts(p) == (1, 0)
            time.sleep(.55)
            assert exchange(s, INIT)[7] == 2
            assert mock_counts(p) == (2, 0)
            time.sleep(.55)
            assert exchange(s, INIT)[7] == 2
            assert mock_counts(p) == (2, 0)


def test_fixed_refresh_failure_clears_previous_route():
    env = {'BONCASRELAY_MOCK_DETECT_INTERVAL_MS': '500',
           'BONCASRELAY_MOCK_AMBIGUOUS_FIXED': '1'}
    fixed = free_port()
    with server('--reader-port', f'C-CAS={fixed}', env=env) as (_, p):
        with connect(fixed) as s:
            assert exchange(s, INIT)[7] == 2
            assert mock_counts(p) == (1, 0)
            assert exchange(s, bytes.fromhex('90 FE 00 00 00')) == b''
            time.sleep(.55)
            # The refreshed name list makes the selector ambiguous. The old
            # successful selection must no longer be usable after this failure.
            assert exchange(s, INIT) == b''
            assert mock_counts(p) == (2, 0)
            burst(s, INIT, 10, b'')
            assert mock_counts(p) == (2, 0)


def test_missing_kind_retries_without_reprobing_healthy_cards():
    env = {'BONCASRELAY_MOCK_DETECT_INTERVAL_MS': '500'}
    bcas_ecm = bytes.fromhex('90 34 00 00 00')
    with server('--acas-reader', 'missing', env=env) as (port, p):
        with connect(port) as s:
            assert exchange(s, bcas_ecm) == b'\xb0\x90\0'
            initial = mock_counts(p)
            assert initial[0] == 1 and initial[1] > 0
            assert exchange(s, ECM) == b''
            assert mock_counts(p) == initial
            time.sleep(.55)
            assert exchange(s, ECM) == b''
            refreshed = (initial[0] + 1, initial[1])
            assert mock_counts(p) == refreshed
            burst(s, ECM, 10, b'')
            assert mock_counts(p) == refreshed
            assert exchange(s, bcas_ecm) == b'\xb0\x90\0'


def test_missing_automatic_kind_discovers_card_replacement():
    env = {'BONCASRELAY_MOCK_DETECT_INTERVAL_MS': '500',
           'BONCASRELAY_MOCK_SWAP_BCAS_TO_ACAS': '1'}
    with server(env=env) as (port, p):
        with connect(port) as s:
            assert exchange(s, INIT)[0] == 0xB0
            assert mock_counts(p) == (1, 2)
            assert exchange(s, ECM) == b''
            assert mock_counts(p) == (1, 2)
            time.sleep(.55)
            # The same reader name now contains ACAS. A cached B-CAS result
            # cannot hide a replacement when the requested kind is missing.
            auth(s, 62)
            assert exchange(s, ECM) == b'\xb1\x3e\x90\0'
            assert mock_counts(p) == (2, 3)


def test_detection_does_not_block_cached_reader():
    env = {'BONCASRELAY_MOCK_DETECT_INTERVAL_MS': '500',
           'BONCASRELAY_MOCK_PROBE_DELAY_MS': '700'}
    bcas_ecm = bytes.fromhex('90 34 00 00 00')
    with server(env=env) as (port, p):
        with connect(port) as bcas, connect(port) as acas:
            # Cold clients share the delayed first scan and all receive a route;
            # only callers with an existing cached route bypass an active scan.
            def cold_request():
                with connect(port) as s:
                    return exchange(s, bcas_ecm)
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                requests = [pool.submit(exchange, bcas, bcas_ecm)]
                requests += [pool.submit(cold_request) for _ in range(3)]
                for request in requests:
                    assert request.result() == b'\xb0\x90\0'
            initial = mock_counts(p)
            assert exchange(acas, bytes.fromhex('90 FE 00 01 00')) == b''
            # ACAS alone needs reclassification. Its probe sleeps for 700 milliseconds.
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                detecting = pool.submit(auth, acas, 99)
                deadline = time.monotonic() + 5
                while mock_counts(p)[1] == initial[1]:
                    assert time.monotonic() < deadline, 'redetection did not start'
                    time.sleep(.005)
                # Socket timeout is below the probe delay: a global routing lock
                # held during the scan would cause this independent reader to fail.
                bcas.settimeout(.35)
                assert exchange(bcas, bcas_ecm) == b'\xb0\x90\0'
                detecting.result()
                assert exchange(acas, ECM) == b'\xb1\x63\x90\0'
            assert mock_counts(p) == (initial[0] + 1, initial[1] + 1)


def main():
    test_detection_cache()
    test_fixed_refresh_failure_clears_previous_route()
    test_missing_kind_retries_without_reprobing_healthy_cards()
    test_missing_automatic_kind_discovers_card_replacement()
    test_detection_does_not_block_cached_reader()
    assert 'Defaults: 0.0.0.0:6900' in subprocess.check_output([EXE, '--help'], text=True)
    assert 'Mock ACAS' in subprocess.check_output([EXE, '--mock', '--list-readers'], text=True)
    for args in [('--port', '0'), ('--port', '65536'), ('--allow', '127.0.0.1/33'),
                 ('--allow', '::1/129'), ('--reader-port', 'bad'), ('--unknown',),
                 ('--reader-port', 'X=6900'), ('--allow', 'garbage')]:
        assert subprocess.run([EXE, *args], capture_output=True).returncode != 0, args

    fixed = free_port()
    with server('--reader-port', f'C-CAS={fixed}', '--allow', '127.0.0.0/8', '--verbose') as (port, p):
        with connect(port) as s:
            # Fragmented header/body, then two pipelined requests.
            frame = bytes([len(INIT)]) + INIT
            for byte in frame:
                s.sendall(bytes([byte]))
            r = reply(s)
            assert len(r) == 59 and r[0] == 0xB0 and r[7] == 5
            s.sendall(frame * 2)
            assert reply(s) == r and reply(s) == r
            assert exchange(s, b'') == b''
            assert exchange(s, bytes.fromhex('90 FD 00 00 00')) == b''  # >255
            assert exchange(s, bytes.fromhex('90 FE 00 00 00')) == b''  # reset
            assert exchange(s, INIT) == r  # same TCP connection recovers
            assert exchange(s, b'\x90\x34\0\0' + bytes(251)) == b'\xb0\x90\0'
            auth(s, 42)
            assert exchange(s, ECM) == bytes([0xB1, 42, 0x90, 0])
        with connect(fixed) as s:
            assert exchange(s, INIT)[7] == 2
            assert exchange(s, bytes.fromhex('90 FE 00 00 00')) == b''
            assert exchange(s, INIT)[7] == 2
            # Even an ACAS APDU goes to C-CAS on this listener.
            assert exchange(s, bytes.fromhex('90 A0 00 01 00')) == b'\x90\0'
        # Different clients cannot interleave auth/ECM on the same reader.
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(lambda t: session(port, t), range(1, 17)))
        # Fixed and automatic listeners share the same per-reader mutex.
        same = free_port()  # tested in another process below
        # Disconnect, idle-auth expiry, and incomplete payload free the transaction.
        with connect(port) as s:
            auth(s, 71)
        session(port, 72)
        with connect(port) as idle:
            auth(idle, 73)
            session(port, 74)  # waits for idle authentication lease to expire
        with connect(port) as slow:
            auth(slow, 75)
            slow.sendall(b'\x05\x90')
            session(port, 76)
        # Graceful shutdown while an idle and an authenticated client remain open.
        with connect(port) as idle, connect(port) as active:
            auth(active, 80)
            stop(p)
            assert idle.recv(1) == b''

    with server('--reader-port', f'ACAS={same}', '--allow', '127.0.0.1/32') as (port, _):
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            a = pool.submit(session, port, 11)
            b = pool.submit(session, same, 12)
            a.result(); b.result()

    with server('--allow', '192.0.2.0/24') as (port, _):
        with connect(port) as s:
            assert s.recv(1) == b''

    with tempfile.TemporaryDirectory() as tmp:
        cfg = Path(tmp) / 'server.ini'
        unused = free_port()
        cfg.write_text(f'[General]\nListenAddress=192.0.2.1\nServerPort={unused}\nAllow=192.0.2.0/24\n'
                       f'[CardReader]\nBCAS=Mock C-CAS\nACAS=Mock ACAS\n[CardPort]\n{unused}=missing\n', encoding='utf-8')
        with server('--config', str(cfg), '--allow', '127.0.0.1/32', '--reader-port', f'C-CAS={free_port()}') as (port, _):
            with connect(port) as s:
                assert exchange(s, INIT)[7] == 2  # INI CardReader pin remains active
        # Verify pinning, missing/ambiguous selector handling and fail-closed behavior.
    with server('--bcas-reader', 'C-CAS') as (port, _):
        with connect(port) as s:
            assert exchange(s, INIT)[7] == 2
    for selector in ('missing', 'Mock'):
        with server('--bcas-reader', selector) as (port, _):
            with connect(port) as s:
                assert exchange(s, INIT) == b''
    # IPv6-only listener and prefix matching, when IPv6 loopback exists.
    try:
        with socket.socket(socket.AF_INET6) as probe:
            probe.bind(('::1', 0))
    except OSError:
        print('IPv6 loopback unavailable; IPv6 TCP test skipped')
    else:
        with server('--allow', '::1/128', host='::1') as (port, _):
            with connect(port, '::1') as s:
                assert exchange(s, INIT)[0] == 0xB0
        with server('--allow', '2001:db8::/32', host='::1') as (port, _):
            with connect(port, '::1') as s:
                assert s.recv(1) == b''
    print('TCP integration OK')


if __name__ == '__main__':
    main()

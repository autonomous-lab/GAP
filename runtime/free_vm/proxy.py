"""Per-VM, loopback-only package egress proxy for QEMU guestfwd.

The QEMU network stays `restrict=on`. Each VM receives its own guestfwd to an
instance of this proxy. The proxy never accepts a destination IP from a guest.
"""
import asyncio
import ipaddress
import re
import socket
import time
from urllib.parse import urlsplit

ALLOWED = frozenset({
    'deb.debian.org', 'security.debian.org',
    'registry.npmjs.org', 'pypi.org', 'files.pythonhosted.org',
    'proxy.golang.org', 'sum.golang.org', 'storage.googleapis.com',
    'repo.packagist.org', 'packagist.org',
    'registry-1.docker.io', 'auth.docker.io', 'production.cloudflare.docker.com',
    'production.cloudfront.docker.com',
    'ghcr.io', 'pkg-containers.githubusercontent.com',
    'github.com', 'api.github.com', 'codeload.github.com',
})
MAX_LINE = 8192
MAX_HEADERS = 16384
MAX_CONNECTION_BYTES = 512 * 1024 * 1024
MAX_VM_BYTES = 10 * 1024 * 1024 * 1024
VM_BYTES_PER_SECOND = 2 * 1024 * 1024


class Denied(Exception):
    pass


class Budget:
    def __init__(self):
        self.total=0
        self.next_time=0.0

    async def charge(self,size):
        self.total+=size
        if self.total>MAX_VM_BYTES:raise Denied('vm_traffic_budget_exhausted')
        self.next_time=max(self.next_time,time.monotonic())+size/VM_BYTES_PER_SECOND
        await asyncio.sleep(max(0,self.next_time-time.monotonic()))


def tls_server_name(record):
    """Read the SNI in a single-record TLS ClientHello; fail closed otherwise."""
    if len(record)<9 or record[0]!=22 or record[1]!=3 or record[2] not in (1,2,3,4):
        raise Denied('tls_client_hello_required')
    if int.from_bytes(record[3:5],'big')!=len(record)-5 or record[5]!=1:
        raise Denied('tls_client_hello_required')
    end=9+int.from_bytes(record[6:9],'big')
    if end>len(record):raise Denied('fragmented_client_hello')
    body=record[9:end]
    if len(body)<35:raise Denied('invalid_client_hello')
    offset=34
    for size_width in (1,2,1):
        if offset+size_width>len(body):raise Denied('invalid_client_hello')
        length=int.from_bytes(body[offset:offset+size_width],'big')
        offset+=size_width+length
        if offset>len(body):raise Denied('invalid_client_hello')
    if offset+2>len(body):raise Denied('sni_required')
    extension_end=offset+2+int.from_bytes(body[offset:offset+2],'big')
    offset+=2
    if extension_end>len(body):raise Denied('invalid_client_hello')
    while offset+4<=extension_end:
        kind=int.from_bytes(body[offset:offset+2],'big')
        length=int.from_bytes(body[offset+2:offset+4],'big')
        offset+=4
        if offset+length>extension_end:raise Denied('invalid_client_hello')
        if kind==0:
            names=body[offset:offset+length]
            if len(names)<5 or int.from_bytes(names[:2],'big')!=len(names)-2 or names[2]!=0:
                raise Denied('invalid_sni')
            name_length=int.from_bytes(names[3:5],'big')
            if 5+name_length>len(names):raise Denied('invalid_sni')
            try:return names[5:5+name_length].decode('ascii').lower().rstrip('.')
            except UnicodeError:raise Denied('invalid_sni') from None
        offset+=length
    raise Denied('sni_required')


def destination(host, port):
    if not isinstance(host, str) or not re.fullmatch(r'[a-z0-9.-]{1,253}', host):
        raise Denied('invalid_host')
    host = host.rstrip('.').lower()
    if host not in ALLOWED:
        raise Denied('host_not_allowed')
    if port not in (443, 80) or (port == 80 and host not in ('deb.debian.org', 'security.debian.org')):
        raise Denied('port_not_allowed')
    return host, port


async def public_address(host, port):
    results = await asyncio.get_running_loop().getaddrinfo(host, port, type=socket.SOCK_STREAM)
    # Pin one DNS result. Never fall back to a private/rebinding answer.
    addresses = [row[4][0] for row in results]
    if not addresses or any(not ipaddress.ip_address(address).is_global for address in addresses):
        raise Denied('non_public_dns_answer')
    # Many hosts publish IPv6 even when the worker has no IPv6 route. Prefer
    # a verified public IPv4 answer; keep IPv6 for IPv6-only registries.
    return next((address for address in addresses if ipaddress.ip_address(address).version == 4), addresses[0])


async def relay(source, destination_writer, allowance, budget=None):
    moved = 0
    while True:
        chunk = await source.read(min(65536, allowance - moved + 1))
        if not chunk:
            break
        moved += len(chunk)
        if moved > allowance:
            raise Denied('connection_byte_limit')
        if budget:await budget.charge(len(chunk))
        destination_writer.write(chunk)
        await destination_writer.drain()


async def handle(reader, writer, budget=None):
    upstream = None
    try:
        first = await asyncio.wait_for(reader.readline(), 10)
        if not first or len(first) > MAX_LINE:
            raise Denied('invalid_request_line')
        parts = first.decode('ascii').strip().split(' ')
        if len(parts) != 3 or parts[2] != 'HTTP/1.1':
            raise Denied('invalid_request_line')
        method, target, _ = parts
        headers = bytearray()
        while True:
            line = await asyncio.wait_for(reader.readline(), 10)
            if not line or len(headers) + len(line) > MAX_HEADERS:
                raise Denied('invalid_headers')
            headers.extend(line)
            if line == b'\r\n':
                break
        if method == 'CONNECT':
            if not re.fullmatch(r'[A-Za-z0-9.-]+:[0-9]{1,5}', target):
                raise Denied('invalid_connect_target')
            host, port = target.rsplit(':', 1)
            host, port = destination(host.lower(), int(port))
            if port != 443:
                raise Denied('tls_only_connect')
            writer.write(b'HTTP/1.1 200 Connection Established\r\n\r\n')
            await writer.drain()
            hello_header=await asyncio.wait_for(reader.readexactly(5),10)
            hello_length=int.from_bytes(hello_header[3:5],'big')
            if hello_length<4 or hello_length>16384:raise Denied('invalid_tls_record')
            hello=hello_header+await asyncio.wait_for(reader.readexactly(hello_length),10)
            if tls_server_name(hello)!=host:raise Denied('tls_sni_mismatch')
            if budget:await budget.charge(len(hello))
            address = await public_address(host, port)
            remote_reader, upstream = await asyncio.wait_for(asyncio.open_connection(address, port), 8)
            upstream.write(hello)
            await upstream.drain()
            tasks = [asyncio.create_task(relay(reader, upstream, MAX_CONNECTION_BYTES,budget)),
                     asyncio.create_task(relay(remote_reader, writer, MAX_CONNECTION_BYTES,budget))]
            done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in pending: task.cancel()
            await asyncio.gather(*pending, return_exceptions=True)
            for task in done: task.result()
        elif method == 'GET':
            parsed = urlsplit(target)
            if parsed.scheme != 'http' or parsed.username or parsed.password or parsed.fragment:
                raise Denied('invalid_http_target')
            host, port = destination(parsed.hostname, parsed.port or 80)
            if port != 80:
                raise Denied('http_only_apt')
            address = await public_address(host, port)
            remote_reader, upstream = await asyncio.wait_for(asyncio.open_connection(address, port), 8)
            path = parsed.path or '/'
            if parsed.query: path += '?' + parsed.query
            # Drop client-supplied headers. Only apt's public GET requests are
            # forwarded, and redirects require a new independently checked URL.
            upstream.write(f'GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n'.encode())
            await upstream.drain()
            await relay(remote_reader, writer, MAX_CONNECTION_BYTES,budget)
        else:
            raise Denied('method_not_allowed')
    except (Denied, ValueError, UnicodeError, asyncio.TimeoutError, asyncio.IncompleteReadError, OSError):
        try:
            writer.write(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            await writer.drain()
        except OSError:
            pass
    finally:
        if upstream:
            upstream.close()
        writer.close()


async def serve(port):
    server = await asyncio.start_server(handle, '127.0.0.1', port, limit=MAX_HEADERS)
    async with server:
        await server.serve_forever()

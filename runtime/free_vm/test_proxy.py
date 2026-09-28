import asyncio
import ssl
import unittest
from unittest.mock import patch

from proxy import Budget, Denied, destination, handle, public_address, tls_server_name


class ProxyTests(unittest.TestCase):
    def test_vm_traffic_budget_is_shared_across_connections(self):
        async def run():
            budget=Budget()
            with patch('proxy.MAX_VM_BYTES',5),patch('proxy.VM_BYTES_PER_SECOND',10**9):
                await budget.charge(3)
                await budget.charge(2)
                with self.assertRaises(Denied):await budget.charge(1)
        asyncio.run(run())

    def test_tls_sni_must_match_the_allowed_connect_host(self):
        context=ssl.create_default_context()
        incoming=ssl.MemoryBIO();outgoing=ssl.MemoryBIO()
        tls=context.wrap_bio(incoming,outgoing,server_hostname='pypi.org')
        with self.assertRaises(ssl.SSLWantReadError):tls.do_handshake()
        hello=outgoing.read()
        self.assertEqual(tls_server_name(hello),'pypi.org')
        with self.assertRaises(Denied):tls_server_name(b'GET / HTTP/1.1\r\n')

    def test_exact_host_and_port_allowlist(self):
        self.assertEqual(destination('registry.npmjs.org', 443), ('registry.npmjs.org',443))
        self.assertEqual(destination('deb.debian.org', 80), ('deb.debian.org',80))
        for host,port in [('169.254.169.254',80),('localhost',443),('npmjs.org.evil.test',443),
                          ('registry.npmjs.org',22),('ghcr.io',80),('8.8.8.8',443)]:
            with self.assertRaises(Denied): destination(host,port)

    def test_nonpublic_dns_fails_closed(self):
        async def run():
            loop=asyncio.get_running_loop()
            original=loop.getaddrinfo
            async def fake(*args, **kwargs): return [(2,1,6,'',('127.0.0.1',443))]
            loop.getaddrinfo=fake
            try:
                with self.assertRaises(Denied): await public_address('ghcr.io',443)
            finally: loop.getaddrinfo=original
        asyncio.run(run())

    def test_proxy_rejects_arbitrary_connect_without_contacting_upstream(self):
        async def run():
            server=await asyncio.start_server(handle,'127.0.0.1',0)
            try:
                reader,writer=await asyncio.open_connection('127.0.0.1',server.sockets[0].getsockname()[1])
                writer.write(b'CONNECT 127.0.0.1:22 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n')
                await writer.drain()
                self.assertTrue((await reader.readline()).startswith(b'HTTP/1.1 403'))
                writer.close();await writer.wait_closed()
            finally:
                server.close();await server.wait_closed()
        asyncio.run(run())

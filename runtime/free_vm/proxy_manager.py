"""One event loop hosts isolated package-proxy listeners for all free VMs."""
import asyncio
import threading

from free_vm_proxy import Budget, handle


class ProxyManager:
    def __init__(self):
        self.guard = threading.Lock()
        self.loop = asyncio.new_event_loop()
        self.ready = threading.Event()
        self.servers = {}
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        if not self.ready.wait(5):
            raise RuntimeError('free_vm_proxy_unavailable')

    def _run(self):
        asyncio.set_event_loop(self.loop)
        self.ready.set()
        self.loop.run_forever()

    async def _listen(self, port, budget):
        slots = asyncio.Semaphore(8)

        async def limited(reader, writer):
            if slots.locked():
                writer.write(b'HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                await writer.drain()
                writer.close()
                return
            async with slots:
                await handle(reader, writer, budget)

        return await asyncio.start_server(limited, '127.0.0.1', port)

    def ensure(self,vm_id,port):
        with self.guard:
            old=self.servers.get(vm_id)
            if old:
                if old[0]!=port or not old[1].is_serving():
                    raise RuntimeError('free_vm_proxy_unavailable')
                return
            try:
                budget=Budget()
                server=asyncio.run_coroutine_threadsafe(self._listen(port,budget),self.loop).result(timeout=5)
            except Exception:
                raise RuntimeError('free_vm_proxy_unavailable') from None
            self.servers[vm_id]=(port,server,budget)

    def release(self,vm_id):
        with self.guard:
            old=self.servers.pop(vm_id,None)
            if not old:return
            async def close():
                old[1].close()
                await old[1].wait_closed()
            asyncio.run_coroutine_threadsafe(close(),self.loop).result(timeout=5)

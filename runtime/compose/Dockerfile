FROM alpine:3.23
RUN apk add --no-cache python3 py3-cryptography py3-asyncssh=2.21.1-r0 sqlcipher-libs openssh-client qemu-system-x86_64 qemu-img \
    qemu-modules e2fsprogs ttyd=1.7.7-r0 && adduser -D -u 10001 runner
ENV LD_PRELOAD=/usr/lib/libsqlcipher.so.0
WORKDIR /opt/runner
COPY runtime/compose/*.py ./
COPY runtime/free_vm/gateway.py ./free_vm_gateway.py
COPY runtime/free_vm/proxy.py ./free_vm_proxy.py
COPY runtime/free_vm/proxy_manager.py ./free_vm_proxy_manager.py
USER 10001:10001
ENTRYPOINT ["python3", "runner.py"]
CMD ["--config", "/config/runner.json", "--bind", "0.0.0.0", "--port", "8092"]

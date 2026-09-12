FROM alpine:3.23
RUN apk add --no-cache python3 openssh-client qemu-system-x86_64 qemu-img \
    qemu-modules e2fsprogs ttyd=1.7.7-r0 && adduser -D -u 10001 runner
WORKDIR /opt/runner
COPY runtime/compose/*.py ./
USER 10001:10001
ENTRYPOINT ["python3", "runner.py"]
CMD ["--config", "/config/runner.json", "--bind", "0.0.0.0", "--port", "8092"]

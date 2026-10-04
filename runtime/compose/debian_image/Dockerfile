# Dedicated Debian base for new paid VMs. Keep the Alpine image immutable for
# existing qcow2 overlays, and keep the free image's proxy policy separate.
FROM debian:trixie-slim AS guest
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    systemd systemd-sysv dbus initramfs-tools linux-image-cloud-amd64 \
    openssh-server docker.io docker-cli docker-compose python3 python3-pip nodejs npm \
    golang-go php-cli php-curl composer git curl ca-certificates iproute2 iputils-ping \
    procps bash vim-tiny make gcc e2fsprogs util-linux && \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/*
COPY runtime/compose/environment.py /usr/local/lib/environment.py
COPY runtime/compose/guest.py /usr/local/lib/gap-compose-guest.py
COPY runtime/free_vm/image/configure.py /tmp/gap-free-configure.py
COPY runtime/compose/debian_image/configure.py /tmp/gap-debian-configure.py
RUN python3 /tmp/gap-free-configure.py && python3 /tmp/gap-debian-configure.py && \
    rm /tmp/gap-free-configure.py /tmp/gap-debian-configure.py && \
    update-initramfs -u -k all && \
    test -f /boot/vmlinuz-$(ls /lib/modules | head -1)

FROM debian:trixie-slim AS pack
RUN apt-get update && apt-get install -y --no-install-recommends e2fsprogs coreutils && rm -rf /var/lib/apt/lists/*
COPY --from=guest / /guest/
RUN mkdir -p /out && \
    cp /guest/boot/vmlinuz-* /out/vmlinuz && \
    cp /guest/boot/initrd.img-* /out/initramfs && \
    truncate -s 6G /out/rootfs.ext4 && \
    mkfs.ext4 -q -F -d /guest /out/rootfs.ext4 && \
    cd /out && sha256sum vmlinuz initramfs rootfs.ext4 > SHA256SUMS && \
    chmod 644 vmlinuz initramfs rootfs.ext4 SHA256SUMS
FROM scratch
COPY --from=pack /out/ /

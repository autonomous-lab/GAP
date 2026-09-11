FROM alpine:3.23 AS build
RUN apk add --no-cache e2fsprogs python3
RUN mkdir /guest /out && apk --root /guest --initdb --no-scripts \
    --keys-dir /etc/apk/keys --repositories-file /etc/apk/repositories add \
    alpine-base linux-virt mkinitfs docker docker-cli-compose docker-cli-buildx docker-openrc \
    openssh openssh-server-common-openrc python3 e2fsprogs-extra
RUN chroot /guest /bin/busybox --install -s
COPY runtime/compose/environment.py /guest/usr/local/lib/environment.py
COPY runtime/compose/guest.py /guest/usr/local/lib/gap-compose-guest.py
COPY runtime/compose/image/seed.init /guest/etc/init.d/gap-seed
COPY runtime/compose/image/configure.py /configure.py
RUN python3 /configure.py && \
    chroot /guest /sbin/depmod -a $(ls /guest/lib/modules) && \
    chroot /guest /sbin/mkinitfs -F "base virtio ext4" $(ls /guest/lib/modules) && \
    cp /guest/boot/vmlinuz-virt /out/vmlinuz && \
    cp /guest/boot/initramfs-virt /out/initramfs && \
    truncate -s 1G /out/rootfs.ext4 && mkfs.ext4 -q -F -d /guest /out/rootfs.ext4 && \
    cd /out && sha256sum vmlinuz initramfs rootfs.ext4 > SHA256SUMS && \
    chmod 644 vmlinuz initramfs rootfs.ext4 SHA256SUMS
FROM scratch
COPY --from=build /out/ /

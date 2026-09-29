#!/bin/sh
# Disposable image smoke test. Run inside a QEMU-equipped container with /dev/kvm.
set -eu
image_dir=${1:-/free-images}
proxy_mode=${2:-off}
case "$proxy_mode" in
    on|off|direct) ;;
    *) echo 'usage: test_boot.sh IMAGE_DIR on|off|direct' >&2; exit 2 ;;
esac
work_dir=$(mktemp -d /tmp/gap-free-boot.XXXXXX)
cleanup() {
    if [ -f "$work_dir/qemu.pid" ]; then
        kill "$(head -1 "$work_dir/qemu.pid")" 2>/dev/null || true
    fi
    rm -rf -- "$work_dir"
}
trap cleanup EXIT INT TERM
if [ "$proxy_mode" = on ]; then
    PYTHONPATH=/ python3 -c 'import asyncio, proxy; asyncio.run(proxy.serve(3128))' &
    proxy_pid=$!
    trap 'kill "$proxy_pid" 2>/dev/null || true; cleanup' EXIT INT TERM
    sleep 1
    proxy_forward=',guestfwd=tcp:10.0.2.100:3128-cmd:/usr/bin/nc 127.0.0.1 3128'
else
    proxy_forward=''
fi
if [ "$proxy_mode" = direct ]; then
    network_restriction=''
else
    network_restriction='restrict=on,'
fi
mkdir "$work_dir/seed"
ssh-keygen -q -t ed25519 -N '' -f "$work_dir/client_key"
ssh-keygen -q -t ed25519 -N '' -f "$work_dir/seed/ssh_host_ed25519_key"
cp "$work_dir/client_key.pub" "$work_dir/seed/authorized_keys"
printf '{}\n' > "$work_dir/seed/runtime.json"
truncate -s 16M "$work_dir/seed.ext4"
mkfs.ext4 -q -F -d "$work_dir/seed" "$work_dir/seed.ext4"
qemu-img create -q -f qcow2 -F raw -b "$image_dir/rootfs.ext4" "$work_dir/disk.qcow2" 8G
qemu-system-x86_64 -machine microvm,accel=kvm -cpu host -m 1024 -smp 1 \
    -kernel "$image_dir/vmlinuz" -initrd "$image_dir/initramfs" \
    -append 'console=ttyS0 root=/dev/vda rootfstype=ext4 modules=virtio_mmio,virtio_blk,ext4 rootwait rw reboot=t net.ifnames=0' \
    -nodefaults -no-user-config -display none -serial "file:$work_dir/serial.log" -no-reboot \
    -sandbox on,obsolete=deny,spawn=allow,resourcecontrol=deny \
    -drive "id=root,file=$work_dir/disk.qcow2,format=qcow2,if=none" -device virtio-blk-device,drive=root \
    -drive "id=seed,file=$work_dir/seed.ext4,format=raw,if=none,readonly=on" -device virtio-blk-device,drive=seed \
    -device virtio-rng-device \
    -netdev "user,id=net0,${network_restriction}hostfwd=tcp:127.0.0.1:22121-:22$proxy_forward" \
    -device virtio-net-device,netdev=net0,mac=52:54:00:12:34:56 \
    -pidfile "$work_dir/qemu.pid" &
sleep 1
attempt=0
while [ "$attempt" -lt 90 ]; do
    if ssh -p 22121 -i "$work_dir/client_key" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=1 -o BatchMode=yes \
        root@127.0.0.1 'for tool in docker python3 node npm go php composer; do command -v "$tool" || exit 1; done; systemctl is-active ssh; docker info --format "{{.ServerVersion}}"' \
        > "$work_dir/result" 2> "$work_dir/ssh-error"; then
        cat "$work_dir/result"
        if [ "$proxy_mode" = on ]; then
            ssh -p 22121 -i "$work_dir/client_key" -o StrictHostKeyChecking=no \
                -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@127.0.0.1 \
                'set -e; echo pypi; curl -fsS --max-time 20 -x http://10.0.2.100:3128 https://pypi.org/simple/pip/ >/dev/null; echo apt; curl -fsS --max-time 20 -x http://10.0.2.100:3128 http://deb.debian.org/debian/README >/dev/null; echo denied; curl -v --max-time 10 -x http://10.0.2.100:3128 https://example.com/ 2>&1 | grep -q "CONNECT tunnel failed, response 403"; echo composer; php -m | grep -qx curl; export HTTP_PROXY=http://10.0.2.100:3128 HTTPS_PROXY=http://10.0.2.100:3128; export http_proxy="$HTTP_PROXY" https_proxy="$HTTPS_PROXY"; COMPOSER_ALLOW_SUPERUSER=1 composer show --all monolog/monolog >/dev/null; echo docker; docker pull hello-world:latest; docker run --rm hello-world:latest' \
                > "$work_dir/network-result" 2> "$work_dir/network-error" || {
                    cat "$work_dir/network-result" >&2
                    cat "$work_dir/network-error" >&2
                    exit 1
                }
            cat "$work_dir/network-result"
        elif [ "$proxy_mode" = direct ]; then
            ssh -p 22121 -i "$work_dir/client_key" -o StrictHostKeyChecking=no \
                -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@127.0.0.1 \
                'ip -4 route show default; getent ahostsv4 example.com; curl --noproxy "*" -fsSI --max-time 15 https://example.com/ | head -1; opencode --version' \
                > "$work_dir/network-result" 2> "$work_dir/network-error" || {
                    cat "$work_dir/network-result" >&2
                    cat "$work_dir/network-error" >&2
                    exit 1
                }
            cat "$work_dir/network-result"
        fi
        exit 0
    fi
    attempt=$((attempt+1))
    sleep 1
done
cat "$work_dir/ssh-error" >&2
ssh -p 22121 -i "$work_dir/client_key" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o ConnectTimeout=1 -o BatchMode=yes \
    root@127.0.0.1 'systemctl status docker --no-pager; journalctl -u docker --no-pager -n 60' >&2 || true
tail -80 "$work_dir/serial.log" >&2
exit 1

"""Read-only Linux KVM probe. No guest boot, disks, networking or installs."""
import fcntl
import json
import os
import platform
import shutil


def check():
    result = {"architecture": platform.machine(), "kvm_api": None,
              "can_create_vm": False, "guest_boot_tested": False,
              "qemu": shutil.which("qemu-system-x86_64"), "ssh": shutil.which("ssh")}
    try:
        device = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
        try:
            result["kvm_api"] = fcntl.ioctl(device, 0xAE00, 0)
            vm = fcntl.ioctl(device, 0xAE01, 0)
            os.close(vm)
            result["can_create_vm"] = True
        finally:
            os.close(device)
    except OSError as error:
        result["error"] = str(error)
    return result


if __name__ == "__main__":
    result = check()
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result["can_create_vm"] else 1)

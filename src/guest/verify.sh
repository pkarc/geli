#!/bin/sh
command -v git >/dev/null || exit 0
command -v node >/dev/null || exit 0
@AGENT_BINARIES@

major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge @NODE_MAJOR@ ] || exit 0

# Without these the host cannot boot the kernel directly, so the image is not publishable.
[ -s /boot/vmlinuz-virt ] || exit 0
[ -s /boot/initramfs-virt ] || exit 0

echo @OK_MARKER@

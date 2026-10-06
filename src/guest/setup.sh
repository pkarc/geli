#!/bin/sh
set -eux

@RETRY@

retry apk update
retry apk add --no-cache bash nodejs npm git python3 py3-pip sudo curl

# The user is created here, not through cloud-init: Alpine's users module cannot set an explicit
# uid and fails the whole module when asked to. The uid has to match the host's, because files
# arrive over 9p owned by the host user — otherwise the agent can read the project but not write.
deluser alpine 2>/dev/null || true
rm -rf /home/alpine
adduser -D -u @HOST_UID@ -s /bin/bash sandbox
printf 'sandbox ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/sandbox
chmod 0440 /etc/sudoers.d/sandbox

# No agent is installed here on purpose. Each one is a qcow2 layer on top of this image, built
# by `geli --build-image --agents` or on first use, so an image only carries the agents its
# owner actually runs. See layer.sh.

install -o sandbox -g sandbox -m 0644 /etc/geli/bash_profile /home/sandbox/.bash_profile

# Autologin on the serial console. Alpine has no systemd, so this is an inittab line plus a
# login helper rather than a getty drop-in. No getty at all: busybox init already opens ttyS0
# as the controlling tty with sane modes, and `getty -n` unconditionally writes a CRLF, which
# was the stray blank line at the top of every session's stdout.
sed -i 's|^ttyS0::respawn:.*|ttyS0::respawn:/usr/local/bin/geli-autologin|' /etc/inittab

# A disposable VM has no use for a clock daemon or an ssh server, and chronyd alone cost ~4s of
# boot slewing the clock the host already provides.
rc-update del chronyd default || true
rc-update del sshd default || true
rc-update del rdate default || true

# QEMU's user networking always hands out the same addresses, so DHCP is pure latency:
# dhcpcd negotiating a lease was most of this image's boot time.
printf 'auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet static\n    address 10.0.2.15\n    netmask 255.255.255.0\n    gateway 10.0.2.2\n' > /etc/network/interfaces
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
printf 'network:\n  config: disabled\n' > /etc/cloud/cloud.cfg.d/99-geli-network.cfg

# Sessions boot the kernel directly, so the boot menu never runs — but a hand-booted image
# still shouldn't sit for 10 seconds waiting for a keypress nobody will make.
sed -i 's/^timeout=.*/timeout=1/' /etc/update-extlinux.conf || true
update-extlinux || true
sed -i 's/^TIMEOUT .*/TIMEOUT 1/; s/^PROMPT .*/PROMPT 0/' /boot/extlinux.conf || true

# The login banner and MOTD are the last guest output the user would see on a clean session.
# Removed rather than emptied: busybox login still prints a newline for an empty motd.
rm -f /etc/motd
: > /etc/issue

# Hand the kernel, the initramfs and the cmdline out to the host: sessions boot them directly
# with -kernel/-initrd, which skips SeaBIOS, iPXE and the bootloader entirely. This is the only
# point where we run as root, and those files are 0600 root-only.
mkdir -p /mnt/geli-out
mount -t 9p -o @MOUNT_OPTS@ @OUT_TAG@ /mnt/geli-out
cp /boot/vmlinuz-virt /mnt/geli-out/@KERNEL@
cp /boot/initramfs-virt /mnt/geli-out/@INITRD@

# Captured, never hardcoded: `root=` depends on how the image labels its filesystem.
{
  printf 'cmdline=%s\n' "$(cat /proc/cmdline)"
  printf 'alpine=%s\n' "$(cut -d' ' -f1-2 /etc/alpine-release 2>/dev/null || echo unknown)"
  printf 'node=%s\n' "$(node --version 2>/dev/null | tr -d v)"
} > /mnt/geli-out/@META@

chown -R @HOST_UID@:@HOST_UID@ /mnt/geli-out
sync
umount /mnt/geli-out

rm -rf /var/cache/apk/*
rm -rf /root/.npm /home/sandbox/.npm /tmp/* 2>/dev/null || true

# Hand the freed blocks back to the qcow2. Deleting files inside the guest does not shrink the
# image on its own.
sync
fstrim -v / || true

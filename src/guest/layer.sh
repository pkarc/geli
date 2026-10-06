#!/bin/sh
set -eux

@RETRY@

@INSTALL@

# cloud-init does not abort runcmd on failure, so the install "finishing" proves nothing. The
# host publishes this layer only if it sees the marker at the bottom of this script, and the
# marker is behind the binary actually existing.
command -v @BINARY@ >/dev/null || exit 0

# Hand the version string out to the host, which keeps one .meta per layer and reads the whole
# chain's worth to print what is inside the image.
mkdir -p /mnt/geli-out
mount -t 9p -o @MOUNT_OPTS@ @OUT_TAG@ /mnt/geli-out

# Piped through `head` on purpose: it trims agents that answer --version with a banner, and it
# also makes the pipeline succeed when the version command does not, which `set -e` would
# otherwise treat as a failed build over a cosmetic string.
version=$( { @VERSION@ ; } 2>/dev/null | head -1 )
[ -n "$version" ] || version=unknown
printf 'agent.@COMMAND@=%s\n' "$version" > /mnt/geli-out/@META@
chown -R @HOST_UID@:@HOST_UID@ /mnt/geli-out
sync
umount /mnt/geli-out

rm -rf /var/cache/apk/* /root/.npm /home/sandbox/.npm 2>/dev/null || true

# Autologin is baked into the base image, so it runs during this build too and the login profile
# drops its "a session already owns this boot" flag before waiting on cloud-init. Baking that
# flag into the layer would make every real session take the nested-shell branch: the command
# would never run and the VM would sit there forever.
rm -f /tmp/.geli-session-active

# Deleting files in the guest does not shrink the qcow2 on its own: blocks written during this
# layer's build stay allocated until the guest discards them. `opencode-ai` alone writes ~540 MB
# of platform variants its recipe then removes, and the image stayed that much larger without
# this. Only this layer's own blocks can be returned — anything in a backing file is not ours.
sync
fstrim -v / || true

echo @OK_MARKER@

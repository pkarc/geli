#!/bin/bash
set -e

echo "============================================="
echo "        GELI SANDBOX ENVIROMENT SETUP        "
echo "============================================="

# 1. Install system dependencies required for QEMU & ISO building on Ubuntu
echo "[*] Checking and installing host system requirements..."
sudo apt update
sudo apt install -y qemu-system-x86 qemu-utils genisoimage curl

# 2. Build the image repository directory
SANDBOX_DIR="$HOME/qemu-sandbox"
mkdir -p "$SANDBOX_DIR"

# 3. Download the Alpine cloud image if it does not exist
IMG_NAME="nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2"
TARGET_IMG="$SANDBOX_DIR/$IMG_NAME"

IMG_URL="https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/cloud/nocloud_alpine-3.22.2-x86_64-bios-cloudinit-r0.qcow2"

if [ ! -f "$TARGET_IMG" ]; then
    echo "[*] Downloading Alpine 3.22 cloud image (approx. 185MB)..."
    curl -L -o "$TARGET_IMG" "$IMG_URL"
    echo "[+] Image downloaded successfully."
else
    echo "[+] Alpine base image already present."
fi

# 4. Build and install the Rust binary globally
echo "[*] Compiling geli release binary..."
cargo build --release

echo "[*] Installing geli to /usr/local/bin/..."
sudo cp target/release/geli /usr/local/bin/

# 5. Provision the base image once, so individual sandbox sessions install nothing.
#    No agent goes in here: each one is a qcow2 layer built the first time you run it, so the
#    image you boot only ever carries the agents you actually use.
echo "[*] Building the base sandbox image (one-time, a few minutes)..."
geli --build-image

echo "============================================="
echo "[✓] Geli installation complete!"
echo "    You can now run: geli <command>"
echo ""
echo "    The first run of an agent builds its layer, once. To get it over with now:"
echo "      geli --build-image --agents claude"
echo "============================================="


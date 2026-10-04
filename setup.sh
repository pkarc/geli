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

# 3. Download the Ubuntu Cloud Image if it doesn't exist
IMG_NAME="ubuntu-24.04-server-cloudimg-amd64.img"
TARGET_IMG="$SANDBOX_DIR/$IMG_NAME"

IMG_URL="https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img"

if [ ! -f "$TARGET_IMG" ]; then
    echo "[*] Downloading Ubuntu 24.04 LTS Gold Master Image (approx. 400MB)..."
    curl -L -o "$TARGET_IMG" "$IMG_URL"
    echo "[+] Image downloaded successfully."
else
    echo "[+] Ubuntu Master Image already present."
fi

# 4. Build and install the Rust binary globally
echo "[*] Compiling geli release binary..."
cargo build --release

echo "[*] Installing geli to /usr/local/bin/..."
sudo cp target/release/geli /usr/local/bin/

# 5. Provision the golden image once, so individual sandbox sessions install nothing
echo "[*] Building the golden sandbox image (one-time, a few minutes)..."
geli --build-image

echo "============================================="
echo "[✓] Geli installation complete!"
echo "    You can now run: geli <command>"
echo "============================================="


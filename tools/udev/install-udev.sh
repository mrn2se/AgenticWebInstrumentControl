#!/bin/bash
set -euo pipefail

# Install udev rules for AWIC
# This script must be run with sudo to copy rules to /etc/udev/rules.d/

if [[ $EUID -ne 0 ]]; then
    echo "This script must be run with sudo to install udev rules."
    echo "Usage: sudo tools/udev/install-udev.sh"
    exit 1
fi

RULES_FILE="$(dirname "$0")/99-awic.rules"
TARGET_DIR="/etc/udev/rules.d"

if [[ ! -f "$RULES_FILE" ]]; then
    echo "Error: $RULES_FILE not found."
    exit 1
fi

echo "Installing udev rules..."
cp "$RULES_FILE" "$TARGET_DIR/"

echo "Reloading udev rules..."
udevadm control --reload-rules
udevadm trigger

echo ""
echo "✓ Udev rules installed successfully."
echo ""
echo "IMPORTANT: Please note the following:"
echo "1. Your user must be a member of the 'plugdev' group."
echo "   Check with: groups"
echo "   If 'plugdev' is not listed, add your user with:"
echo "   sudo usermod -a -G plugdev \$USER"
echo "   Then log out and back in for the group membership to take effect."
echo ""
echo "2. If you have USB instruments already connected, you must replug them"
echo "   for the new permissions to take effect."
echo ""

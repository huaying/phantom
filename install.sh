#!/bin/sh
# Phantom Remote Desktop — install script
# Usage: curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
#
# Installs phantom-server and/or phantom-client to /usr/local/bin.
# On Linux, also installs required runtime libraries.

set -e

REPO="huaying/phantom"
INSTALL_DIR="/usr/local/bin"

# --- Detect OS and Arch ---
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)  OS="linux" ;;
    darwin) OS="macos" ;;
    *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "Detected: ${OS}/${ARCH}"

# --- Determine what to install ---
INSTALL_SERVER=false
INSTALL_CLIENT=false
AUTOLOGIN=false
GOT_ROLE=false

for arg in "$@"; do
    case "$arg" in
        server) INSTALL_SERVER=true; GOT_ROLE=true ;;
        client) INSTALL_CLIENT=true; GOT_ROLE=true ;;
        both)   INSTALL_SERVER=true; INSTALL_CLIENT=true; GOT_ROLE=true ;;
        --autologin) AUTOLOGIN=true ;;
        *) echo "Unknown argument: $arg"; echo "Usage: $0 [server|client|both] [--autologin]"; exit 1 ;;
    esac
done

if [ "$GOT_ROLE" = false ]; then
    # Default: server on Linux, client on macOS
    case "$OS" in
        linux) INSTALL_SERVER=true ;;
        macos) INSTALL_CLIENT=true ;;
    esac
fi

if [ "$AUTOLOGIN" = true ] && { [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ]; }; then
    echo "--autologin only applies to Linux server installs; ignoring"
    AUTOLOGIN=false
fi

# --- Install Linux runtime dependencies ---
if [ "$OS" = "linux" ]; then
    echo "Installing runtime dependencies..."

    if command -v apt-get > /dev/null 2>&1; then
        # Debian / Ubuntu
        PKGS=""
        if [ "$INSTALL_SERVER" = true ]; then
            PKGS="libxcb1 libxcb-shm0 libxcb-randr0 libxtst6 libxdo3 libpulse0"
        fi
        if [ "$INSTALL_CLIENT" = true ]; then
            # Client: winit needs xcb + xcb-randr (multi-monitor), softbuffer
            # renders via xcb-shm, alsa for audio output.
            PKGS="$PKGS libxcb1 libxcb-shm0 libxcb-randr0 libasound2"
        fi
        if [ -n "$PKGS" ]; then
            sudo apt-get update -qq
            sudo apt-get install -y --no-install-recommends $PKGS || true
        fi

    elif command -v dnf > /dev/null 2>&1; then
        # Fedora / RHEL
        PKGS=""
        if [ "$INSTALL_SERVER" = true ]; then
            PKGS="libxcb libxdo libXtst pulseaudio-libs"
        fi
        if [ "$INSTALL_CLIENT" = true ]; then
            PKGS="$PKGS libxcb alsa-lib"
        fi
        if [ -n "$PKGS" ]; then
            sudo dnf install -y $PKGS || true
        fi

    elif command -v pacman > /dev/null 2>&1; then
        # Arch Linux
        PKGS=""
        if [ "$INSTALL_SERVER" = true ]; then
            PKGS="libxcb xdotool libxtst libpulse"
        fi
        if [ "$INSTALL_CLIENT" = true ]; then
            PKGS="$PKGS libxcb alsa-lib"
        fi
        if [ -n "$PKGS" ]; then
            sudo pacman -S --needed --noconfirm $PKGS || true
        fi

    else
        echo "Warning: could not detect package manager. You may need to install runtime libraries manually."
        echo "  Server: libxcb, libxdo, libpulse"
        echo "  Client: libasound (ALSA)"
    fi
fi

# --- Get latest release URL ---
BASE_URL="https://github.com/${REPO}/releases/latest/download"

download_and_install() {
    name="$1"
    url="${BASE_URL}/${name}-${OS}-${ARCH}"

    echo "Downloading ${name}..."
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL "$url" -o "/tmp/${name}"
    elif command -v wget > /dev/null 2>&1; then
        wget -qO "/tmp/${name}" "$url"
    else
        echo "Error: curl or wget required"; exit 1
    fi

    chmod +x "/tmp/${name}"

    # Install — use sudo if needed
    if [ -w "$INSTALL_DIR" ]; then
        mv "/tmp/${name}" "${INSTALL_DIR}/${name}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "/tmp/${name}" "${INSTALL_DIR}/${name}"
    fi

    echo "Installed: ${INSTALL_DIR}/${name}"
}

# --- Install ---
if [ "$INSTALL_SERVER" = true ]; then
    download_and_install "phantom-server"
fi

if [ "$INSTALL_CLIENT" = true ]; then
    download_and_install "phantom-client"
fi

# --- Linux server: configure /dev/uinput for keyboard injection ---
# Server uses /dev/uinput to create a virtual keyboard (bypasses the
# X11 XKB remap path that scrambles keys on GDM 42, and also works on
# Wayland + lock screens where XTest can't reach). Needs:
#   1. udev rule giving the `input` group rw on /dev/uinput
#   2. invoking user in the `input` group
# Without this the server still runs but falls back to enigo/XTest,
# with a loud warning in logs and the known GDM-42 scramble bug
# lurking.
if [ "$OS" = "linux" ] && [ "$INSTALL_SERVER" = true ]; then
    echo ""
    echo "Configuring /dev/uinput for keyboard injection..."
    UDEV_RULE_PATH="/etc/udev/rules.d/99-phantom-uinput.rules"
    UDEV_RULE_CONTENT='KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"'

    # Only write if missing or different (idempotent re-install)
    if [ ! -f "$UDEV_RULE_PATH" ] || ! grep -qxF "$UDEV_RULE_CONTENT" "$UDEV_RULE_PATH" 2>/dev/null; then
        echo "$UDEV_RULE_CONTENT" | sudo tee "$UDEV_RULE_PATH" > /dev/null
        sudo udevadm control --reload-rules
        sudo udevadm trigger /dev/uinput 2>/dev/null || true
        echo "  Wrote $UDEV_RULE_PATH"
    else
        echo "  udev rule already in place"
    fi

    # Add invoking user to input group. SUDO_USER preferred when
    # install.sh is piped through sudo; fall back to $USER.
    TARGET_USER="${SUDO_USER:-$USER}"
    if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
        if id -nG "$TARGET_USER" 2>/dev/null | grep -qw input; then
            echo "  User $TARGET_USER already in 'input' group"
        else
            sudo usermod -a -G input "$TARGET_USER"
            echo "  Added $TARGET_USER to 'input' group"
            echo ""
            echo "⚠️  Log out and back in for the 'input' group to take effect,"
            echo "   or run 'newgrp input' in your current shell before starting"
            echo "   phantom-server. Otherwise keyboard injection falls back to"
            echo "   XTest (login screen typing may be unreliable on Ubuntu 22)."
        fi
    fi
fi

# --- Linux server --autologin: configure GDM autologin + disable screen lock
# + auto-unlock keyring. Target use case is remote VMs where the phantom
# session needs to survive user sign out (Windows-style service feel).
# Without autologin, the X session dies on sign out and phantom-server can't
# reattach. See docs/pitfalls.md for the full rationale.
if [ "$OS" = "linux" ] && [ "$INSTALL_SERVER" = true ] && [ "$AUTOLOGIN" = true ]; then
    echo ""
    echo "Configuring auto-login (per --autologin)..."

    TARGET_USER="${SUDO_USER:-$USER}"
    if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = "root" ]; then
        echo "  ERROR: cannot determine non-root user for autologin. Re-run as a regular user via sudo."
        exit 1
    fi
    USER_HOME=$(getent passwd "$TARGET_USER" | cut -d: -f6)
    if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
        echo "  ERROR: could not find home directory for $TARGET_USER"
        exit 1
    fi

    # 1. GDM autologin (Ubuntu 22/24 default DM)
    if [ -f /etc/gdm3/custom.conf ]; then
        # Back up original once so we can revert cleanly later
        if [ ! -f /etc/gdm3/custom.conf.phantom-bak ]; then
            sudo cp /etc/gdm3/custom.conf /etc/gdm3/custom.conf.phantom-bak
        fi
        # AutomaticLogin only fires at boot. After the user signs out, GDM
        # falls back to the greeter and stays there. TimedLogin kicks the
        # greeter back into auto-login after a short delay — that's what
        # makes "sign out = immediate new session" work for a dedicated
        # remote-access box.
        sudo tee /etc/gdm3/custom.conf > /dev/null <<EOF
# Written by phantom install.sh --autologin
# Original backed up at /etc/gdm3/custom.conf.phantom-bak
[daemon]
AutomaticLoginEnable = true
AutomaticLogin = $TARGET_USER
TimedLoginEnable = true
TimedLogin = $TARGET_USER
TimedLoginDelay = 5

[security]

[xdmcp]

[chooser]

[debug]
EOF
        echo "  Enabled GDM autologin for $TARGET_USER"
    else
        echo "  WARN: /etc/gdm3/custom.conf not found. Only GDM is supported here — configure autologin manually for your DM."
    fi

    # 2. Disable GNOME screen lock + idle (system-wide dconf override so it
    #    applies before the user ever logs in and picks it up on every boot).
    sudo mkdir -p /etc/dconf/profile /etc/dconf/db/local.d
    if [ ! -f /etc/dconf/profile/user ]; then
        sudo tee /etc/dconf/profile/user > /dev/null <<EOF
user-db:user
system-db:local
EOF
    fi
    sudo tee /etc/dconf/db/local.d/00-phantom-no-lock > /dev/null <<'EOF'
[org/gnome/desktop/screensaver]
lock-enabled=false
idle-activation-enabled=false

[org/gnome/desktop/session]
idle-delay=uint32 0
EOF
    sudo dconf update 2>/dev/null || true
    echo "  Disabled GNOME screen lock + idle timeout"

    # 3. Clear any pre-existing keyring. The login keyring is encrypted with
    #    the user's password — under autologin, PAM never captures that
    #    password so the keyring can't be unlocked and Chrome/etc. pop up a
    #    dialog asking for it. Wiping it forces gnome-keyring-daemon to
    #    create a fresh, empty-password keyring on next session (via step 4).
    #    Trade-off: any pre-existing keyring contents are lost. Fresh VMs
    #    have nothing in there, so this is usually a no-op.
    if [ -d "$USER_HOME/.local/share/keyrings" ]; then
        sudo rm -rf "$USER_HOME/.local/share/keyrings"
        echo "  Cleared existing keyring (fresh empty one created on next login)"
    fi

    # 4. Autostart hook: every session start, hand gnome-keyring-daemon an
    #    empty password via stdin. If no keyring exists, it creates one with
    #    no password → stays unlocked forever, no popup.
    AUTOSTART_DIR="$USER_HOME/.config/autostart"
    sudo -u "$TARGET_USER" mkdir -p "$AUTOSTART_DIR"
    sudo -u "$TARGET_USER" tee "$AUTOSTART_DIR/phantom-keyring-unlock.desktop" > /dev/null <<'EOF'
[Desktop Entry]
Type=Application
Name=Phantom Unlock Keyring
Comment=Unlock gnome-keyring with empty password so autologin sessions don't pop up a keyring dialog
Exec=sh -c 'printf "" | gnome-keyring-daemon --unlock'
X-GNOME-Autostart-enabled=true
X-GNOME-Autostart-Phase=Initialization
NoDisplay=true
EOF
    echo "  Installed keyring auto-unlock autostart entry"

    # 5. phantom-server autostart. GDM assigns a fresh DISPLAY number to
    #    each new session (sign out + TimedLogin fire = :0 → :1 → :2 ...),
    #    so a daemon pinned to DISPLAY=:0 breaks after the first sign-out.
    #    Launching from XDG autostart gives us the right DISPLAY and
    #    XAUTHORITY for free, one instance per session. This is the Linux
    #    analogue of Windows Service mode: session lifecycle drives
    #    phantom-server, with GDM autologin+TimedLogin making sure there's
    #    always a session.
    # NOTE on the Exec= wrapper: phantom-server from a previous autologin
    # session can survive past the session (gets reparented to init when
    # gnome-session exits) and keep ports 9900/9901 bound. The new
    # session's autostart would then bind-fail silently. Wrapper kills
    # stale instances first, then launches fresh on the current DISPLAY.
    sudo -u "$TARGET_USER" tee "$AUTOSTART_DIR/phantom-server.desktop" > /dev/null <<'EOF'
[Desktop Entry]
Type=Application
Name=Phantom Server
Comment=Remote-desktop server. Edit Exec= below to change transport/encryption/auth.
Exec=sh -c 'pkill -x phantom-server 2>/dev/null; for i in 1 2 3 4 5; do pgrep -x phantom-server >/dev/null 2>&1 || break; sleep 1; done; exec /usr/local/bin/phantom-server --no-encrypt --transport tcp,web'
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF
    echo "  Installed phantom-server autostart entry (edit ~/.config/autostart/phantom-server.desktop to change flags)"

    # 6. Watchdog timer. GDM 42 on Ubuntu 22.04 has a regression where
    #    TimedLogin doesn't fire reliably after sign-out — the greeter
    #    just sits there forever. Our workaround: poll every 30s, and if
    #    no $TARGET_USER seat0 session exists, kick gdm3 (restart
    #    re-triggers AutomaticLogin from scratch). Belt-and-suspenders
    #    on U24 where TimedLogin does work natively.
    sudo tee /usr/local/bin/phantom-autologin-watchdog.sh > /dev/null <<EOF
#!/bin/sh
# Kick gdm3 if there is no active seat0 session for $TARGET_USER.
# Written by phantom install.sh --autologin.
SID=\$(loginctl list-sessions --no-legend | awk '\$3=="$TARGET_USER" && \$4=="seat0" && !/closing/{print \$1}')
if [ -z "\$SID" ]; then
    logger "phantom-autologin-watchdog: no $TARGET_USER seat0, restarting gdm3"
    systemctl restart gdm3
fi
EOF
    sudo chmod +x /usr/local/bin/phantom-autologin-watchdog.sh
    sudo tee /etc/systemd/system/phantom-autologin-watchdog.service > /dev/null <<EOF
[Unit]
Description=Re-trigger GDM autologin for $TARGET_USER if no seat0 session exists
After=gdm3.service

[Service]
Type=oneshot
ExecStart=/usr/local/bin/phantom-autologin-watchdog.sh
EOF
    sudo tee /etc/systemd/system/phantom-autologin-watchdog.timer > /dev/null <<'EOF'
[Unit]
Description=Poll every 30s and kick gdm3 if autologin fails to re-fire

[Timer]
OnBootSec=2min
OnUnitActiveSec=30s
Unit=phantom-autologin-watchdog.service

[Install]
WantedBy=timers.target
EOF
    sudo systemctl daemon-reload
    sudo systemctl enable --now phantom-autologin-watchdog.timer > /dev/null 2>&1
    echo "  Installed autologin watchdog timer (workaround for GDM 42 TimedLogin regression)"

    echo ""
    echo "⚠️  Autologin takes effect on next reboot. Security note: the console"
    echo "   will no longer require a password, and the keyring will be stored"
    echo "   unencrypted. This is intended for dedicated remote-access VMs."
fi

# --- Post-install hints ---
echo ""
echo "Done!"
if [ "$INSTALL_SERVER" = true ]; then
    echo ""
    echo "Start server:"
    echo "  phantom-server"
    echo "  # TCP:9900 (native client) + Web:9901 (browser: https://localhost:9901)"
    echo ""
    echo "With GPU (NVIDIA):"
    echo "  DISPLAY=:0 phantom-server --capture nvfbc --encoder nvenc"
fi
if [ "$INSTALL_CLIENT" = true ]; then
    echo ""
    echo "Connect to server:"
    echo "  phantom-client -c <server-ip>:9900"
fi

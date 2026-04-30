#!/bin/sh
# Phantom Remote Desktop — install script
# Usage: curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
#
# Installs phantom-server and/or phantom-client to /usr/local/bin by default.
# On Linux, also installs required runtime libraries.

set -e

REPO="huaying/phantom"
INSTALL_DIR="${PHANTOM_INSTALL_DIR:-/usr/local/bin}"
# Test/CI can override the source without changing installer behavior:
#   PHANTOM_SERVER_BIN=/tmp/phantom-server
#   PHANTOM_CLIENT_BIN=/tmp/phantom-client
#   PHANTOM_LOCAL_ASSET_DIR=/tmp/phantom-assets
#   PHANTOM_ASSET_BASE_URL=http://10.0.0.1:8000
#   PHANTOM_INSTALL_DIR=/tmp/phantom-bin
BASE_URL="${PHANTOM_ASSET_BASE_URL:-https://github.com/${REPO}/releases/latest/download}"
BASE_URL="${BASE_URL%/}"

# ===========================================================================
# Generic helpers
# ===========================================================================

have_cmd() {
    command -v "$1" > /dev/null 2>&1
}

detect_os_arch() {
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
}

parse_args() {
    INSTALL_SERVER=false
    INSTALL_CLIENT=false
    AUTOLOGIN=false
    LIGHT_GUI=false
    LIGHT_GUI_FORCE=false
    SSO=false
    NO_AUTOSTART=false
    RUN_DOCTOR=true
    DOCTOR_ONLY=false
    DOCTOR_STRICT=false
    DOCTOR_CAPTURE_TIMEOUT=8
    GOT_ROLE=false
    INSTALL_MODE="auto"
    DISPLAY_PROFILE_RESULT="not-run"
    DISPLAY_PROFILE_REASON="light-gui path not executed"
    TARGET_USER_ARG=""

    for _arg in "$@"; do
        case "$_arg" in
            server) INSTALL_SERVER=true; GOT_ROLE=true ;;
            client) INSTALL_CLIENT=true; GOT_ROLE=true ;;
            both)   INSTALL_SERVER=true; INSTALL_CLIENT=true; GOT_ROLE=true ;;
            --autologin) AUTOLOGIN=true ;;
            --light-gui) LIGHT_GUI=true; LIGHT_GUI_FORCE=true ;;
            --sso)  SSO=true ;;
            --no-autostart) NO_AUTOSTART=true ;;
            --gpu-strict) INSTALL_MODE="gpu-strict" ;;
            --safe-display) INSTALL_MODE="safe" ;;
            --install-mode=*) INSTALL_MODE="${_arg#--install-mode=}" ;;
            --user=*) TARGET_USER_ARG="${_arg#--user=}" ;;
            --target-user=*) TARGET_USER_ARG="${_arg#--target-user=}" ;;
            --doctor) DOCTOR_ONLY=true; RUN_DOCTOR=true ;;
            --no-doctor) RUN_DOCTOR=false ;;
            --doctor-strict) DOCTOR_STRICT=true ;;
            --doctor-timeout=*) DOCTOR_CAPTURE_TIMEOUT="${_arg#--doctor-timeout=}" ;;
            *) echo "Unknown argument: $_arg"; echo "Usage: $0 [server|client|both] [--user=<login>] [--autologin] [--light-gui] [--sso] [--no-autostart] [--install-mode=auto|gpu-strict|safe] [--doctor|--no-doctor|--doctor-strict]"; exit 1 ;;
        esac
    done
}

apply_defaults() {
    if [ "$DOCTOR_ONLY" = true ]; then
        INSTALL_SERVER=false
        INSTALL_CLIENT=false
        GOT_ROLE=true
    fi

    if [ "$GOT_ROLE" = false ]; then
        # Default: server on Linux, client on macOS
        case "$OS" in
            linux) INSTALL_SERVER=true ;;
            macos) INSTALL_CLIENT=true ;;
        esac
    fi

    if [ "$SSO" = true ] && { [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ]; }; then
        echo "--sso only applies to Linux server installs; ignoring"
        SSO=false
    fi

    if [ "$AUTOLOGIN" = true ] && { [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ]; }; then
        echo "--autologin only applies to Linux server installs; ignoring"
        AUTOLOGIN=false
    fi

    if [ "$LIGHT_GUI" = true ] && { [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ]; }; then
        echo "--light-gui only applies to Linux server installs; ignoring"
        LIGHT_GUI=false
    fi

    case "$INSTALL_MODE" in
        auto|gpu-strict|safe) ;;
        *)
            echo "Invalid --install-mode: $INSTALL_MODE"
            echo "Expected one of: auto, gpu-strict, safe"
            exit 1
            ;;
    esac

    if [ "$INSTALL_MODE" != "auto" ] && { [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ]; }; then
        echo "--install-mode=$INSTALL_MODE only applies to Linux server installs; ignoring"
        INSTALL_MODE="auto"
    fi

    # In autologin mode, a desktop stack is required for a recoverable remote
    # session. Enable lightweight GUI bootstrap automatically.
    if [ "$AUTOLOGIN" = true ] && [ "$OS" = "linux" ] && [ "$INSTALL_SERVER" = true ]; then
        LIGHT_GUI=true
    fi
}

# Resolve the invoking non-root user and their home directory. SUDO_USER
# is set when install.sh is piped through sudo; fall back to $USER.
# USER_HOME is used by the autostart and autologin steps to write files
# under ~/.config/autostart, ~/.local/share/keyrings, etc.
get_target_user() {
    if [ -n "$TARGET_USER_ARG" ]; then
        TARGET_USER="$TARGET_USER_ARG"
    elif [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
        TARGET_USER="$SUDO_USER"
    elif [ -n "${USER:-}" ] && [ "$USER" != "root" ]; then
        TARGET_USER="$USER"
    elif have_cmd getent; then
        TARGET_USER=$(getent passwd | awk -F: '$3 >= 1000 && $3 < 60000 && $1 != "nobody" && $6 ~ /^\// {print $1; exit}')
    else
        TARGET_USER=""
    fi
    USER_HOME=""
    if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
        if have_cmd getent; then
            USER_HOME=$(getent passwd "$TARGET_USER" 2>/dev/null | cut -d: -f6 || true)
        fi
    fi
    if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
        USER_HOME="$HOME"
    fi
}

download_and_install() {
    _name="$1"
    _tmp="$(mktemp "/tmp/${_name}.XXXXXX")"
    _asset="${_name}-${OS}-${ARCH}"
    _url="${BASE_URL}/${_asset}"

    _direct=""
    case "$_name" in
        phantom-server) _direct="${PHANTOM_SERVER_BIN:-}" ;;
        phantom-client) _direct="${PHANTOM_CLIENT_BIN:-}" ;;
    esac

    if [ -n "$_direct" ]; then
        if [ ! -f "$_direct" ]; then
            echo "Error: local Phantom binary not found: $_direct"
            exit 1
        fi
        echo "Using local ${_name}: $_direct"
        cp "$_direct" "$_tmp"
    elif [ -n "${PHANTOM_LOCAL_ASSET_DIR:-}" ]; then
        if [ -f "${PHANTOM_LOCAL_ASSET_DIR}/${_asset}" ]; then
            echo "Using local asset: ${PHANTOM_LOCAL_ASSET_DIR}/${_asset}"
            cp "${PHANTOM_LOCAL_ASSET_DIR}/${_asset}" "$_tmp"
        elif [ -f "${PHANTOM_LOCAL_ASSET_DIR}/${_name}" ]; then
            echo "Using local asset: ${PHANTOM_LOCAL_ASSET_DIR}/${_name}"
            cp "${PHANTOM_LOCAL_ASSET_DIR}/${_name}" "$_tmp"
        else
            echo "Error: ${_asset} not found in PHANTOM_LOCAL_ASSET_DIR=${PHANTOM_LOCAL_ASSET_DIR}"
            exit 1
        fi
    else
        echo "Downloading ${_name}..."
        if have_cmd curl; then
            curl -fsSL "$_url" -o "$_tmp"
        elif have_cmd wget; then
            wget -qO "$_tmp" "$_url"
        else
            echo "Error: curl or wget required"; exit 1
        fi
    fi

    chmod +x "$_tmp"

    # Install — use sudo if needed
    if [ -w "$INSTALL_DIR" ]; then
        mv "$_tmp" "${INSTALL_DIR}/${_name}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "$_tmp" "${INSTALL_DIR}/${_name}"
    fi

    echo "Installed: ${INSTALL_DIR}/${_name}"
}

# ===========================================================================
# Linux: runtime package install
# ===========================================================================

linux_install_deps() {
    echo "Installing runtime dependencies..."

    if have_cmd apt-get; then
        linux_install_deps_apt
    elif have_cmd dnf; then
        linux_install_deps_dnf
    elif have_cmd pacman; then
        linux_install_deps_pacman
    else
        echo "Warning: could not detect package manager. You may need to install runtime libraries manually."
        echo "  Server: libxcb, libxdo, libpulse"
        echo "  Client: libasound (ALSA)"
    fi
}

linux_apt_update_best_effort() {
    if ! sudo apt-get update -qq; then
        echo "Warning: apt-get update failed; continuing with existing package indexes."
        echo "         Check /etc/apt/sources.list.d if package installation fails below."
    fi
}

linux_install_deps_apt() {
    # Debian / Ubuntu
    _pkgs=""
    if [ "$INSTALL_SERVER" = true ]; then
        _pkgs="libxcb1 libxcb-shm0 libxcb-randr0 libxtst6 libxdo3 libpulse0"
    fi
    if [ "$INSTALL_CLIENT" = true ]; then
        # Client: winit needs xcb + xcb-randr (multi-monitor), softbuffer
        # renders via xcb-shm, alsa for audio output.
        _pkgs="$_pkgs libxcb1 libxcb-shm0 libxcb-randr0 libasound2"
    fi
    if [ -n "$_pkgs" ]; then
        linux_apt_update_best_effort
        # shellcheck disable=SC2086 # package list must split into separate args
        sudo apt-get install -y --no-install-recommends $_pkgs || true
    fi
}

linux_install_deps_dnf() {
    # Fedora / RHEL
    _pkgs=""
    if [ "$INSTALL_SERVER" = true ]; then
        _pkgs="libxcb libxdo libXtst pulseaudio-libs"
    fi
    if [ "$INSTALL_CLIENT" = true ]; then
        _pkgs="$_pkgs libxcb alsa-lib"
    fi
    if [ -n "$_pkgs" ]; then
        # shellcheck disable=SC2086 # package list must split into separate args
        sudo dnf install -y $_pkgs || true
    fi
}

linux_install_deps_pacman() {
    # Arch Linux
    _pkgs=""
    if [ "$INSTALL_SERVER" = true ]; then
        _pkgs="libxcb xdotool libxtst libpulse"
    fi
    if [ "$INSTALL_CLIENT" = true ]; then
        _pkgs="$_pkgs libxcb alsa-lib"
    fi
    if [ -n "$_pkgs" ]; then
        # shellcheck disable=SC2086 # package list must split into separate args
        sudo pacman -S --needed --noconfirm $_pkgs || true
    fi
}

# ===========================================================================
# Linux server: optional lightweight GUI bootstrap (XFCE + LightDM)
# ===========================================================================
# For headless Ubuntu/Debian VMs, "install + run" often fails because no
# display manager / desktop session exists, or Xorg cannot build a screen.
# --light-gui installs a minimal desktop stack and configures a headless
# display profile when no physical monitor is connected.
# Profile choice is controlled by --install-mode:
#   auto (default): prefer NVIDIA headless profile on NVIDIA VMs; use dummy only without NVIDIA
#   gpu-strict: require NVIDIA headless profile; never write dummy
#   safe: always force dummy profile

linux_has_connected_display() {
    for _status in /sys/class/drm/*/status; do
        [ -f "$_status" ] || continue
        if grep -qx "connected" "$_status" 2>/dev/null; then
            return 0
        fi
    done
    return 1
}

linux_has_nvidia_gpu() {
    if ! have_cmd nvidia-smi; then
        return 1
    fi
    nvidia-smi -L > /dev/null 2>&1
}

linux_nvidia_display_active() {
    if ! have_cmd nvidia-smi; then
        return 1
    fi
    nvidia-smi --query-gpu=display_active --format=csv,noheader 2>/dev/null | grep -qi "Enabled"
}

linux_has_dummy_xorg_conf() {
    [ -f /etc/X11/xorg.conf ] && grep -qi 'Driver[[:space:]]*"dummy"' /etc/X11/xorg.conf 2>/dev/null
}

linux_xrandr_query() {
    DISPLAY=:0 xrandr --query 2>/dev/null && return 0

    if [ -n "$USER_HOME" ] && [ -f "$USER_HOME/.Xauthority" ] && have_cmd sudo; then
        sudo -u "$TARGET_USER" env DISPLAY=:0 XAUTHORITY="$USER_HOME/.Xauthority" xrandr --query 2>/dev/null && return 0
    fi

    if have_cmd sudo; then
        for _auth in $(sudo find /run/user \( -path '*/gdm/Xauthority' -o -path '*/lightdm/Xauthority' \) -type f 2>/dev/null || true); do
            sudo env DISPLAY=:0 XAUTHORITY="$_auth" xrandr --query 2>/dev/null && return 0
        done
    fi

    for _auth in /run/user/*/gdm/Xauthority /run/user/*/lightdm/Xauthority; do
        [ -f "$_auth" ] || continue
        XAUTHORITY="$_auth" DISPLAY=:0 xrandr --query 2>/dev/null && return 0
    done

    return 1
}

linux_xorg_has_nvidia_output() {
    linux_xrandr_query | grep -Eq '^DP-[0-9]+ connected|^HDMI-[0-9]+ connected|^DVI-[0-9]+ connected'
}

linux_nvidia_profile_healthy() {
    linux_nvidia_display_active && linux_xorg_has_nvidia_output
}

linux_wait_nvidia_profile_healthy() {
    _deadline=$(( $(date +%s) + 30 ))
    while [ "$(date +%s)" -lt "$_deadline" ]; do
        if linux_nvidia_profile_healthy; then
            return 0
        fi
        sleep 2
    done
    linux_nvidia_profile_healthy
}

linux_restart_display_manager() {
    _dm="display-manager"
    if systemctl is-enabled lightdm > /dev/null 2>&1 || systemctl is-active lightdm > /dev/null 2>&1; then
        _dm="lightdm"
    elif systemctl is-enabled gdm3 > /dev/null 2>&1 || systemctl is-active gdm3 > /dev/null 2>&1; then
        _dm="gdm3"
    fi
    sudo systemctl restart "$_dm" > /dev/null 2>&1 || sudo systemctl restart display-manager > /dev/null 2>&1 || true
    sleep 4
}

linux_install_dummy_xorg_conf() {
    echo "  No connected monitor detected; configuring dummy Xorg screen (1920x1080)..."
    if [ -f /etc/X11/xorg.conf ] && [ ! -f /etc/X11/xorg.conf.phantom-bak ]; then
        sudo cp /etc/X11/xorg.conf /etc/X11/xorg.conf.phantom-bak
    fi
    sudo tee /etc/X11/xorg.conf > /dev/null <<'EOF'
# Written by phantom install.sh --light-gui
Section "ServerLayout"
    Identifier "Layout0"
    Screen 0 "Screen0"
EndSection

Section "Monitor"
    Identifier "Monitor0"
    HorizSync 28.0-80.0
    VertRefresh 48.0-75.0
EndSection

Section "Device"
    Identifier "Device0"
    Driver "dummy"
    VideoRam 256000
EndSection

Section "Screen"
    Identifier "Screen0"
    Device "Device0"
    Monitor "Monitor0"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1920x1080"
    EndSubSection
EndSection
EOF
}

linux_install_nvidia_edid_file() {
    _edid_hex='00 ff ff ff ff ff ff 00 0d 82 70 27 0f 2c 9b 03 0e 18 01 04 a2 00
01 78 fb 6e a5 a3 54 4f 9f 26 11 50 54 a5 6b 80 61 c0 81 c0 81 00
8b c0 8c c0 a9 c0 a9 40 b3 00 a9 36 80 b8 71 38 2d 40 58 58 45 00
80 38 74 00 00 1e 00 00 00 fc 00 66 69 74 48 65 61 64 6c 65 73 73
34 6b 2b 32 00 a0 f0 70 23 80 31 20 36 00 00 70 f8 00 00 18 70 17
40 a0 b0 08 2d 70 08 60 22 01 40 08 b7 00 00 18 01 0b 02 03 09 00
44 05 81 0f 04 70 17 00 a0 a0 40 2d 60 08 60 22 01 00 40 a6 00 00
18 70 17 00 a0 a0 a0 2d 50 08 60 22 01 00 a0 a5 00 00 18 70 17 00
a0 80 00 2d 60 08 60 22 01 00 00 86 00 00 18 d7 09 50 a0 50 00 2d
30 08 60 22 01 50 00 53 00 00 18 b0 68 56 a0 50 00 2e 30 30 20 36
00 56 00 53 00 00 1c 30 2a f8 c0 f1 00 24 90 40 80 13 00 f8 00 f9
00 00 1e 00 00 00 00 00 00 00 00 00 8e de'
    sudo mkdir -p /etc/X11
    if have_cmd xxd; then
        printf "%s\n" "$_edid_hex" | tr -d ' \n' | xxd -r -p | sudo tee /etc/X11/fitHeadless4k.edid > /dev/null
    else
        echo "  WARN: xxd not found; writing EDID in text form."
        printf "%s\n" "$_edid_hex" | sudo tee /etc/X11/fitHeadless4k.edid > /dev/null
    fi
    sudo chmod 644 /etc/X11/fitHeadless4k.edid
}

linux_disable_dummy_xorg_if_present() {
    if linux_has_dummy_xorg_conf; then
        sudo cp /etc/X11/xorg.conf /etc/X11/xorg.conf.disabled-dummy 2>/dev/null || true
        sudo rm -f /etc/X11/xorg.conf
        echo "  Disabled dummy /etc/X11/xorg.conf to allow NVIDIA display ownership."
    fi
}

linux_install_nvidia_headless_xorg_conf() {
    sudo mkdir -p /etc/X11/xorg.conf.d
    sudo tee /etc/X11/xorg.conf.d/90-nvidia.conf > /dev/null <<'EOF'
Section "OutputClass"
    Identifier "nvidia"
    MatchDriver "nvidia-drm"
    Driver "nvidia"
    Option "PrimaryGPU" "true"
    Option "ModeDebug" "true"
    Option "ConnectToAcpid" "false"
    Option "UseDisplayDevice" "DFP"
    Option "CustomEDID" "DFP-0:/etc/X11/fitHeadless4k.edid"
    Option "ConnectedMonitor" "DFP-0"
    Option "SLI" "Mosaic"
    ModulePath "/usr/lib/x86_64-linux-gnu/xorg/modules"
EndSection
EOF
}

linux_try_nvidia_headless_profile() {
    echo "  No physical display detected; trying NVIDIA headless profile..."
    linux_install_nvidia_edid_file
    linux_install_nvidia_headless_xorg_conf
    linux_disable_dummy_xorg_if_present
    linux_restart_display_manager

    if linux_wait_nvidia_profile_healthy; then
        echo "  NVIDIA headless profile is healthy (display_active=Enabled, DP output connected)."
        return 0
    fi

    _active="$(nvidia-smi --query-gpu=display_active,display_mode --format=csv,noheader 2>/dev/null | head -n1 || true)"
    _xr="$(linux_xrandr_query 2>/dev/null | head -n3 || true)"
    echo "  NVIDIA headless profile validation failed."
    [ -n "$_active" ] && echo "    nvidia-smi: $_active"
    [ -n "$_xr" ] && echo "    xrandr:"
    [ -n "$_xr" ] && echo "$_xr" | sed 's/^/      /'
    return 1
}

linux_setup_headless_display_profile() {
    if [ "$INSTALL_MODE" = "safe" ]; then
        linux_setup_headless_dummy_apt
        DISPLAY_PROFILE_RESULT="dummy"
        DISPLAY_PROFILE_REASON="forced by --install-mode=safe"
        return 0
    fi

    if linux_has_nvidia_gpu; then
        if linux_try_nvidia_headless_profile; then
            DISPLAY_PROFILE_RESULT="nvidia-headless"
            DISPLAY_PROFILE_REASON="NVIDIA profile healthy"
            return 0
        fi

        echo "  WARN: NVIDIA headless profile was installed but is not healthy yet."
        echo "        Keeping NVIDIA config in place; not writing dummy xorg.conf on an NVIDIA VM."
        echo "        Reboot once, then run: install.sh --doctor --doctor-strict --install-mode=$INSTALL_MODE"
        DISPLAY_PROFILE_RESULT="nvidia-headless-pending"
        DISPLAY_PROFILE_REASON="NVIDIA profile installed; live validation did not become healthy before timeout"
        return 0
    fi

    if [ "$INSTALL_MODE" = "gpu-strict" ]; then
        DISPLAY_PROFILE_RESULT="failed"
        DISPLAY_PROFILE_REASON="gpu-strict requested but no NVIDIA GPU detected"
        echo "ERROR: --install-mode=gpu-strict requested, but no NVIDIA GPU was detected."
        exit 1
    fi

    linux_setup_headless_dummy_apt
    DISPLAY_PROFILE_RESULT="dummy"
    DISPLAY_PROFILE_REASON="no NVIDIA GPU detected"
}

linux_install_light_gui_apt() {
    echo ""
    echo "Installing lightweight GUI stack (XFCE + LightDM)..."
    linux_apt_update_best_effort
    echo "lightdm shared/default-x-display-manager select lightdm" | sudo debconf-set-selections || true
    echo "/etc/X11/default-display-manager string /usr/sbin/lightdm" | sudo debconf-set-selections || true
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        xorg xfce4 lightdm xserver-xorg-video-dummy dbus-x11 x11-xserver-utils || true

    echo "/usr/sbin/lightdm" | sudo tee /etc/X11/default-display-manager > /dev/null
    sudo mkdir -p /etc/lightdm/lightdm.conf.d
    sudo tee /etc/lightdm/lightdm.conf.d/50-phantom-xfce.conf > /dev/null <<'EOF'
[Seat:*]
user-session=xfce
autologin-session=xfce
EOF

    if [ "$AUTOLOGIN" = true ] && [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
        sudo tee /etc/lightdm/lightdm.conf.d/60-phantom-autologin.conf > /dev/null <<EOF
[Seat:*]
autologin-user=$TARGET_USER
autologin-user-timeout=0
EOF
    fi

    if linux_has_connected_display; then
        echo "  Physical display detected; skipping dummy Xorg config."
        DISPLAY_PROFILE_RESULT="physical-display"
        DISPLAY_PROFILE_REASON="monitor connected; no headless profile required"
    else
        linux_setup_headless_display_profile
    fi

    sudo systemctl disable gdm3 > /dev/null 2>&1 || true
    sudo systemctl enable lightdm > /dev/null 2>&1 || true
    echo "  LightDM enabled (applies after reboot)."
}

linux_setup_headless_dummy_apt() {
    echo ""
    echo "Installing headless X fallback (dummy screen)..."
    linux_apt_update_best_effort
    sudo apt-get install -y --no-install-recommends xserver-xorg-video-dummy || true
    linux_install_dummy_xorg_conf
}

linux_has_display_manager() {
    if [ -f /etc/gdm3/custom.conf ] || [ -d /etc/lightdm ] || [ -f /etc/lightdm/lightdm.conf ]; then
        return 0
    fi
    return 1
}

linux_setup_light_gui_if_requested() {
    if [ "$LIGHT_GUI" != true ] || [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ]; then
        return 0
    fi

    if have_cmd apt-get; then
        if [ "$LIGHT_GUI_FORCE" = true ] || ! linux_has_display_manager; then
            linux_install_light_gui_apt
        elif linux_has_connected_display; then
            echo ""
            echo "Display manager already installed and monitor is connected; skipping --light-gui bootstrap."
            DISPLAY_PROFILE_RESULT="physical-display"
            DISPLAY_PROFILE_REASON="display manager present and monitor connected"
        else
            linux_setup_headless_display_profile
        fi
    else
        echo ""
        echo "WARN: --light-gui currently auto-installs only on apt-based distros."
        echo "      Install manually: XFCE + LightDM + xserver dummy driver."
    fi
}

# ===========================================================================
# Linux server: /dev/uinput for keyboard injection
# ===========================================================================
# Server uses /dev/uinput to create a virtual keyboard (bypasses the
# X11 XKB remap path that scrambles keys on GDM 42, and also works on
# Wayland + lock screens where XTest can't reach). Needs:
#   1. udev rule giving the `input` group rw on /dev/uinput
#   2. invoking user in the `input` group
# Without this the server still runs but falls back to enigo/XTest,
# with a loud warning in logs and the known GDM-42 scramble bug
# lurking.

linux_configure_uinput() {
    echo ""
    echo "Configuring /dev/uinput for keyboard injection..."
    _udev_rule_path="/etc/udev/rules.d/99-phantom-uinput.rules"
    _udev_rule_content='KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"'

    # Only write if missing or different (idempotent re-install)
    if [ ! -f "$_udev_rule_path" ] || ! grep -qxF "$_udev_rule_content" "$_udev_rule_path" 2>/dev/null; then
        echo "$_udev_rule_content" | sudo tee "$_udev_rule_path" > /dev/null
        sudo udevadm control --reload-rules
        if have_cmd modprobe; then
            sudo modprobe uinput 2>/dev/null || true
        fi
        sudo udevadm trigger /dev/uinput 2>/dev/null || true
        echo "  Wrote $_udev_rule_path"
    else
        echo "  udev rule already in place"
        if have_cmd modprobe; then
            sudo modprobe uinput 2>/dev/null || true
        fi
    fi

    # Add invoking user to input group. SUDO_USER preferred when
    # install.sh is piped through sudo; fall back to $USER.
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
}

# ===========================================================================
# Linux server autostart: XDG autostart entry so phantom-server launches
# whenever the user starts a graphical session. This is the default path
# (opt out with --no-autostart) and is safer than a plain
# `phantom-server --install` systemd user unit pinned to DISPLAY=:0,
# because GDM rotates DISPLAY per session (sign out → :0 → :1 → ...) and
# a pinned unit breaks after the first sign-out. XDG autostart gives us
# DISPLAY + XAUTHORITY + seat from the live session for free.
# ===========================================================================

linux_install_autostart() {
    echo ""
    echo "Installing phantom-server autostart entry..."
    if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
        echo "  WARN: could not resolve home directory for $TARGET_USER — skipping autostart."
        echo "        Start manually with: phantom-server"
        return 0
    fi
    _autostart_dir="$USER_HOME/.config/autostart"
    # NOTE on the Exec= wrapper: phantom-server from a previous autologin
    # session can survive past the session (gets reparented to init when
    # gnome-session exits) and keep ports 9900/9901 bound. The new
    # session's autostart would then bind-fail silently. Wrapper kills
    # stale instances first, then launches fresh on the current DISPLAY.
    sudo -u "$TARGET_USER" mkdir -p "$_autostart_dir"
    sudo -u "$TARGET_USER" tee "$_autostart_dir/phantom-server.desktop" > /dev/null <<'EOF'
[Desktop Entry]
Type=Application
Name=Phantom Server
Comment=Remote-desktop server. Edit Exec= below to change transport/encryption/auth.
Exec=sh -c 'pkill -x phantom-server 2>/dev/null; for i in 1 2 3 4 5; do pgrep -x phantom-server >/dev/null 2>&1 || break; sleep 1; done; exec /usr/local/bin/phantom-server --no-encrypt --transport tcp,web'
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF
    echo "  Wrote $_autostart_dir/phantom-server.desktop"
    echo "  phantom-server will start at your next graphical login."
    echo "  Edit Exec= in that file to change transport / encryption / auth flags."
}

linux_resolve_xauthority_for_display() {
    _display="$1"
    _uid=""
    if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
        _uid="$(id -u "$TARGET_USER" 2>/dev/null || true)"
    fi

    for _auth in \
        "$USER_HOME/.Xauthority" \
        "/run/user/$_uid/gdm/Xauthority" \
        "/run/user/$_uid/lightdm/Xauthority"
    do
        [ -n "$_auth" ] && [ -f "$_auth" ] || continue
        if have_cmd xrandr && have_cmd sudo; then
            if sudo -u "$TARGET_USER" env DISPLAY="$_display" XAUTHORITY="$_auth" xrandr --query >/dev/null 2>&1; then
                echo "$_auth"
                return 0
            fi
        else
            echo "$_auth"
            return 0
        fi
    done

    return 1
}

linux_start_server_now() {
    if [ "$OS" != "linux" ] || [ "$INSTALL_SERVER" != true ] || [ "$NO_AUTOSTART" = true ]; then
        return 0
    fi
    if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = "root" ] || [ -z "$USER_HOME" ]; then
        echo ""
        echo "WARN: cannot resolve non-root graphical user; skipping immediate phantom-server start."
        return 0
    fi

    _bin="$(linux_phantom_server_bin || true)"
    if [ -z "$_bin" ] || [ ! -x "$_bin" ]; then
        echo ""
        echo "WARN: phantom-server binary not found; skipping immediate start."
        return 0
    fi

    _display="${PHANTOM_SERVER_DISPLAY:-:0}"
    if [ ! -S "/tmp/.X11-unix/X${_display#:}" ]; then
        echo ""
        echo "WARN: DISPLAY=$_display is not available yet; phantom-server will start at next graphical login."
        return 0
    fi

    _xauth="$(linux_resolve_xauthority_for_display "$_display" || true)"
    if [ -z "$_xauth" ]; then
        echo ""
        echo "WARN: could not find a working Xauthority for $TARGET_USER on DISPLAY=$_display."
        echo "      phantom-server will start at next graphical login."
        return 0
    fi

    _uid="$(id -u "$TARGET_USER" 2>/dev/null || true)"
    _runtime="/run/user/$_uid"
    _log_dir="$USER_HOME/.local/state/phantom"
    _log="$_log_dir/phantom-server.log"

    echo ""
    echo "Starting phantom-server in the current graphical session..."
    sudo -u "$TARGET_USER" mkdir -p "$_log_dir"
    sudo -u "$TARGET_USER" env \
        HOME="$USER_HOME" \
        DISPLAY="$_display" \
        XAUTHORITY="$_xauth" \
        XDG_RUNTIME_DIR="$_runtime" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=$_runtime/bus" \
        sh -c '
            pkill -x phantom-server 2>/dev/null || true
            for i in 1 2 3 4 5; do
                pgrep -x phantom-server >/dev/null 2>&1 || break
                sleep 1
            done
            nohup "$1" --no-encrypt --transport tcp,web >> "$2" 2>&1 &
        ' sh "$_bin" "$_log"

    for _i in 1 2 3 4 5 6 7 8 9 10; do
        if pgrep -u "$_uid" -x phantom-server >/dev/null 2>&1 && linux_doctor_port_listening 9901; then
            echo "  phantom-server started (log: $_log)"
            return 0
        fi
        sleep 1
    done

    echo "  WARN: phantom-server did not become ready immediately; check $_log"
}

# ===========================================================================
# Linux server --autologin: GDM autologin + disable screen lock
# + auto-unlock keyring. Target use case is remote VMs where the phantom
# session needs to survive user sign out (Windows-style service feel).
# Without autologin, the X session dies on sign out and phantom-server can't
# reattach. See docs/pitfalls.md for the full rationale.
# ===========================================================================

linux_configure_autologin() {
    echo ""
    echo "Configuring auto-login (per --autologin)..."

    if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = "root" ]; then
        echo "  ERROR: cannot determine non-root user for autologin. Re-run as a regular user via sudo."
        exit 1
    fi
    if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
        echo "  ERROR: could not find home directory for $TARGET_USER"
        exit 1
    fi

    linux_autologin_configure_display_manager
    linux_autologin_disable_screenlock
    linux_autologin_reset_keyring
    linux_autologin_install_keyring_unlock
    # Autostart entry is already installed by the default path; --autologin
    # just layers on GDM autologin + screen-lock disable + watchdog.
    linux_autologin_install_watchdog

    echo ""
    echo "⚠️  Autologin takes effect on next reboot. Security note: the console"
    echo "   will no longer require a password, and the keyring will be stored"
    echo "   unencrypted. This is intended for dedicated remote-access VMs."
}

linux_autologin_configure_display_manager() {
    _default_dm="$(basename "$(cat /etc/X11/default-display-manager 2>/dev/null || true)")"

    if [ "$_default_dm" = "lightdm" ] \
        || systemctl is-active --quiet lightdm 2>/dev/null \
        || systemctl is-enabled --quiet lightdm 2>/dev/null; then
        linux_autologin_lightdm
        return 0
    fi

    if [ "$_default_dm" = "gdm3" ] || [ "$_default_dm" = "gdm" ] \
        || systemctl is-active --quiet gdm3 2>/dev/null \
        || systemctl is-active --quiet gdm 2>/dev/null \
        || systemctl is-enabled --quiet gdm3 2>/dev/null \
        || systemctl is-enabled --quiet gdm 2>/dev/null; then
        linux_autologin_gdm
        return 0
    fi

    if [ -d /etc/lightdm ] || [ -f /etc/lightdm/lightdm.conf ]; then
        linux_autologin_lightdm
        return 0
    fi

    linux_autologin_gdm
}

linux_autologin_lightdm() {
    echo "/usr/sbin/lightdm" | sudo tee /etc/X11/default-display-manager > /dev/null
    sudo mkdir -p /etc/lightdm/lightdm.conf.d
    sudo tee /etc/lightdm/lightdm.conf.d/60-phantom-autologin.conf > /dev/null <<EOF
# Written by phantom install.sh --autologin
[Seat:*]
autologin-user=$TARGET_USER
autologin-user-timeout=0
autologin-session=xfce
user-session=xfce
EOF
    sudo systemctl enable lightdm > /dev/null 2>&1 || true
    echo "  Enabled LightDM autologin for $TARGET_USER"
}

linux_autologin_gdm() {
    # 1. GDM autologin (Ubuntu 22/24 default DM)
    if [ -f /etc/gdm3/custom.conf ]; then
        echo "/usr/sbin/gdm3" | sudo tee /etc/X11/default-display-manager > /dev/null
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
        sudo systemctl enable gdm3 > /dev/null 2>&1 || true
        echo "  Enabled GDM autologin for $TARGET_USER"
    else
        echo "  WARN: /etc/gdm3/custom.conf not found. Only GDM is supported here — configure autologin manually for your DM."
    fi
}

linux_autologin_disable_screenlock() {
    # 2. Disable GNOME screen lock + idle + user switching (system-wide dconf
    #    override so it applies before the user ever logs in and picks it up
    #    on every boot).
    #
    #    disable-user-switching: blocks the "Switch User" menu item. Without
    #    this, clicking Switch User leaves the original user's X session
    #    locked + backgrounded on its VT while GDM spawns a greeter on a new
    #    VT. phantom stays pinned to DISPLAY=:0 (the backgrounded session)
    #    and keeps streaming a black screen; autologin can't recover because
    #    the original session isn't technically dead.
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

[org/gnome/desktop/lockdown]
disable-lock-screen=true
disable-user-switching=true
EOF
    sudo dconf update 2>/dev/null || true

    # XFCE/lightdm images often start light-locker from XDG autostart. If it
    # locks the session, LightDM switches the active seat to a greeter VT while
    # phantom keeps streaming the backgrounded desktop, which commonly appears
    # black through NVFBC.
    _autostart_dir="$USER_HOME/.config/autostart"
    sudo -u "$TARGET_USER" mkdir -p "$_autostart_dir"
    for _locker_desktop in light-locker.desktop xfce4-screensaver.desktop org.xfce.ScreenSaver.desktop gnome-screensaver.desktop; do
        sudo -u "$TARGET_USER" tee "$_autostart_dir/$_locker_desktop" > /dev/null <<EOF
[Desktop Entry]
Type=Application
Name=Disabled by Phantom
Hidden=true
X-GNOME-Autostart-enabled=false
EOF
    done
    sudo -u "$TARGET_USER" tee "$_autostart_dir/phantom-no-screenlock.desktop" > /dev/null <<'EOF'
[Desktop Entry]
Type=Application
Name=Phantom No Screen Lock
Comment=Keep the autologin desktop visible for remote capture.
Exec=sh -c 'xset s off -dpms s noblank 2>/dev/null || true; xfconf-query -c xfce4-session -p /general/LockCommand -s "" --create -t string 2>/dev/null || true; (sleep 5; pkill -x light-locker 2>/dev/null || true; pkill -x xfce4-screensav 2>/dev/null || true; pkill -x gnome-screensav 2>/dev/null || true; xset s off -dpms s noblank 2>/dev/null || true) &'
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF

    sudo pkill -x light-locker 2>/dev/null || true
    sudo pkill -x xfce4-screensav 2>/dev/null || true
    sudo pkill -x gnome-screensav 2>/dev/null || true
    echo "  Disabled GNOME/XFCE screen lock + idle timeout + user switching"
}

linux_autologin_reset_keyring() {
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
}

linux_autologin_install_keyring_unlock() {
    # 4. Autostart hook: every session start, hand gnome-keyring-daemon an
    #    empty password via stdin. If no keyring exists, it creates one with
    #    no password → stays unlocked forever, no popup.
    _autostart_dir="$USER_HOME/.config/autostart"
    sudo -u "$TARGET_USER" mkdir -p "$_autostart_dir"
    sudo -u "$TARGET_USER" tee "$_autostart_dir/phantom-keyring-unlock.desktop" > /dev/null <<'EOF'
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
}

linux_autologin_install_watchdog() {
    # 6. Watchdog timer. GDM 42 on Ubuntu 22.04 has a regression where
    #    TimedLogin doesn't fire reliably after sign-out — the greeter
    #    just sits there forever. Our workaround: poll every 30s, and if
    #    no $TARGET_USER seat0 session exists, kick gdm3 (restart
    #    re-triggers AutomaticLogin from scratch). Belt-and-suspenders
    #    on U24 where TimedLogin does work natively.
    sudo tee /usr/local/bin/phantom-autologin-watchdog.sh > /dev/null <<EOF
#!/bin/sh
# Kick display manager if there is no active seat0 session for $TARGET_USER.
# Written by phantom install.sh --autologin.
SID=\$(loginctl list-sessions --no-legend | awk '\$3=="$TARGET_USER" && \$4=="seat0" && !/closing/{print \$1}')
if [ -z "\$SID" ]; then
    DM=display-manager
    if systemctl is-enabled lightdm > /dev/null 2>&1 || systemctl is-active lightdm > /dev/null 2>&1; then
        DM=lightdm
    elif systemctl is-enabled gdm3 > /dev/null 2>&1 || systemctl is-active gdm3 > /dev/null 2>&1; then
        DM=gdm3
    fi
    logger "phantom-autologin-watchdog: no $TARGET_USER seat0, restarting \$DM"
    systemctl restart "\$DM"
fi
EOF
    sudo chmod +x /usr/local/bin/phantom-autologin-watchdog.sh
    sudo tee /etc/systemd/system/phantom-autologin-watchdog.service > /dev/null <<EOF
[Unit]
Description=Re-trigger display-manager autologin for $TARGET_USER if no seat0 session exists
After=display-manager.service

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
}

# ===========================================================================
# Post-install hints
# ===========================================================================

print_post_install_hints() {
    echo ""
    echo "Done!"
    if [ "$INSTALL_SERVER" = true ] && [ "$OS" = "linux" ]; then
        echo ""
        if [ "$LIGHT_GUI" = true ]; then
            echo "Display profile result: $DISPLAY_PROFILE_RESULT"
            echo "Display profile reason: $DISPLAY_PROFILE_REASON"
            echo "Install mode: $INSTALL_MODE"
            if [ "$DISPLAY_PROFILE_RESULT" = "nvidia-headless-pending" ]; then
                echo "NVIDIA profile is installed but the current graphical session did not validate yet."
                echo "Reboot once, then verify with: install.sh --doctor --doctor-strict --install-mode=$INSTALL_MODE"
            fi
            echo ""
        fi
        if [ "$NO_AUTOSTART" = false ]; then
            if [ "$AUTOLOGIN" = true ]; then
                echo "Server will auto-start after the next reboot (via GDM autologin)."
                echo "Access it at: TCP:9900 (native client) / https://<host>:9901 (browser)"
            else
                echo "Server will auto-start at your next graphical login."
                echo "To start it now in the current session:"
                echo "  phantom-server"
                echo "  # TCP:9900 (native client) + https://localhost:9901 (browser)"
            fi
        else
            echo "Start server manually:"
            echo "  phantom-server"
            echo "  # TCP:9900 (native client) + https://localhost:9901 (browser)"
        fi
        echo ""
        echo "With GPU (NVIDIA):"
        echo "  DISPLAY=:0 phantom-server --capture nvfbc --encoder nvenc"
    elif [ "$INSTALL_SERVER" = true ]; then
        echo ""
        echo "Start server:"
        echo "  phantom-server"
        echo "  # TCP:9900 (native client) + https://localhost:9901 (browser)"
    fi
    if [ "$INSTALL_CLIENT" = true ]; then
        echo ""
        echo "Connect to server:"
        echo "  phantom-client -c <server-ip>:9900"
    fi
}

# ===========================================================================
# Linux SSO plugin (pam_phantom.so + /etc/pam.d/gdm-password patch)
# ===========================================================================
# After phantom-server verifies a JWT, it writes /run/phantom/auth. GDM's
# PAM stack asks pam_phantom.so first (`auth sufficient`); the module matches
# the ticket against PAM_USER and, on success, short-circuits the whole
# stack — no password prompt shown.
#
# This function requires a full source checkout (cargo + libpam-dev + the
# pam-phantom crate at ./crates/pam-phantom). Release builds don't ship
# the .so yet — Phase 2 will add it to release.yml.

linux_install_sso() {
    echo "Installing SSO plugin (per --sso)..."

    if [ ! -d "./crates/pam-phantom" ]; then
        echo "  ERROR: ./crates/pam-phantom not found — run --sso from a phantom source checkout."
        return 1
    fi
    if ! dpkg -s libpam0g-dev > /dev/null 2>&1; then
        echo "  Installing libpam0g-dev..."
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y libpam0g-dev
    fi

    TARGET_USER=${SUDO_USER:-$(logname 2>/dev/null || whoami)}
    if [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = "root" ]; then
        echo "  ERROR: cannot determine non-root user to own /run/phantom."
        return 1
    fi

    # Build as the invoking user — root usually lacks rustup / cargo on PATH,
    # and anything under ~/.cargo belongs to TARGET_USER anyway.
    CARGO_BIN="/home/${TARGET_USER}/.cargo/bin/cargo"
    if [ ! -x "$CARGO_BIN" ] && ! have_cmd cargo; then
        echo "  ERROR: cargo not found (tried $CARGO_BIN and \$PATH)."
        echo "  Install rustup (https://rustup.rs) as $TARGET_USER and re-run."
        return 1
    fi

    echo "  Building libpam_phantom.so as $TARGET_USER..."
    sudo -u "$TARGET_USER" bash -c "cd '$(pwd)/crates/pam-phantom' && '${CARGO_BIN}' build --release 2>&1 | tail -4"
    SO="./crates/pam-phantom/target/release/libpam_phantom.so"
    if [ ! -f "$SO" ]; then
        echo "  ERROR: $SO did not build."
        return 1
    fi

    SO_DST="/lib/x86_64-linux-gnu/security/pam_phantom.so"
    sudo cp "$SO" "$SO_DST"
    sudo chmod 644 "$SO_DST"
    echo "  Installed $SO_DST"

    # tmpfiles.d: recreate /run/phantom on every boot (tmpfs is volatile),
    # owned by the phantom-server user so the agent can write the ticket.
    TMPFILES=/etc/tmpfiles.d/phantom.conf
    printf 'd /run/phantom 0755 %s %s\n' "$TARGET_USER" "$TARGET_USER" | sudo tee "$TMPFILES" > /dev/null
    sudo systemd-tmpfiles --create "$TMPFILES" > /dev/null 2>&1 || true
    echo "  tmpfiles.d: /run/phantom on boot (owner $TARGET_USER)"

    # Patch /etc/pam.d/gdm-password — insert `auth sufficient pam_phantom.so`
    # ABOVE the `@include common-auth` line. Backup first; skip if already
    # present. If the line isn't found, insert after the first `auth` line.
    PAMD=/etc/pam.d/gdm-password
    if [ -f "$PAMD" ]; then
        if sudo grep -q pam_phantom "$PAMD"; then
            echo "  $PAMD already references pam_phantom — leaving alone"
        else
            sudo cp "$PAMD" "${PAMD}.phantom-backup"
            if sudo grep -q '^@include common-auth' "$PAMD"; then
                sudo sed -i '/^@include common-auth/i auth sufficient pam_phantom.so' "$PAMD"
            else
                sudo sed -i '0,/^auth/s//auth sufficient pam_phantom.so\n&/' "$PAMD"
            fi
            echo "  Patched $PAMD (backup at ${PAMD}.phantom-backup)"
        fi
    else
        echo "  $PAMD not found — skipping (non-GDM display manager?)"
    fi

    echo "  Done. Remember:"
    echo "    1. Build phantom-server with --features sso (cargo build --release -p phantom-server --features sso)"
    echo "    2. Launch phantom-server with --auth-secret <hex>"
    echo "    3. Connect with a JWT carrying \"sub\"=<target user>; PAM will pick up the ticket"
}

# ===========================================================================
# Linux server doctor
# ===========================================================================
# The installer can set up packages and config files, but a remote desktop VM
# is only usable when the OS display stack is ready. The doctor turns that
# hidden state into explicit pass/warn/fail output so cloud-init and humans can
# tell whether the next step is "reboot", "fix GPU display", or "start server".

doctor_reset() {
    DOCTOR_FAILS=0
    DOCTOR_WARNS=0
    DOCTOR_REBOOT_REQUIRED=false
}

doctor_ok() {
    echo "  OK: $*"
}

doctor_warn() {
    DOCTOR_WARNS=$((DOCTOR_WARNS + 1))
    echo "  WARN: $*"
}

doctor_fail() {
    DOCTOR_FAILS=$((DOCTOR_FAILS + 1))
    echo "  FAIL: $*"
}

doctor_reboot() {
    DOCTOR_REBOOT_REQUIRED=true
    doctor_warn "$* (reboot or graphical re-login required)"
}

linux_phantom_server_bin() {
    if have_cmd phantom-server; then
        command -v phantom-server
        return 0
    fi
    if [ -x "$INSTALL_DIR/phantom-server" ]; then
        echo "$INSTALL_DIR/phantom-server"
        return 0
    fi
    return 1
}

linux_doctor_run_as_user() {
    _doctor_display="${PHANTOM_DOCTOR_DISPLAY:-:0}"
    _doctor_home="${USER_HOME:-$HOME}"
    _current_user="$(id -un 2>/dev/null || true)"
    if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ] && [ "$_current_user" != "$TARGET_USER" ] && have_cmd sudo; then
        sudo -u "$TARGET_USER" env \
            HOME="$_doctor_home" \
            DISPLAY="$_doctor_display" \
            XAUTHORITY="$_doctor_home/.Xauthority" \
            "$@"
    else
        env \
            HOME="$_doctor_home" \
            DISPLAY="$_doctor_display" \
            XAUTHORITY="$_doctor_home/.Xauthority" \
            "$@"
    fi
}

linux_doctor_first_line() {
    printf "%s\n" "$1" | sed -n '1p'
}

linux_doctor_port_listening() {
    _port="$1"
    if have_cmd ss; then
        ss -ltn 2>/dev/null | awk '{print $4}' | grep -Eq "[:.]${_port}$"
        return $?
    fi
    if have_cmd lsof; then
        lsof -nP -iTCP:"$_port" -sTCP:LISTEN >/dev/null 2>&1
        return $?
    fi
    return 2
}

linux_doctor_runtime_deps() {
    _bin="$1"
    if ! have_cmd ldd; then
        doctor_warn "ldd not found; skipping runtime dependency check"
        return 0
    fi
    _missing="$(ldd "$_bin" 2>/dev/null | grep 'not found' || true)"
    if [ -z "$_missing" ]; then
        doctor_ok "runtime shared-library dependencies are present"
    else
        doctor_fail "missing runtime libraries:"
        printf "%s\n" "$_missing" | sed 's/^/    /'
    fi
}

linux_doctor_display_stack() {
    if linux_has_display_manager; then
        doctor_ok "display manager config found"
    else
        doctor_warn "no GDM/LightDM config found; headless VMs need --light-gui or --autologin"
    fi

    if have_cmd systemctl; then
        if systemctl is-active --quiet lightdm 2>/dev/null; then
            doctor_ok "LightDM is active"
        elif systemctl is-active --quiet gdm3 2>/dev/null; then
            doctor_ok "GDM is active"
        elif systemctl is-active --quiet display-manager 2>/dev/null; then
            doctor_ok "display-manager is active"
        elif [ "$LIGHT_GUI" = true ] || [ "$AUTOLOGIN" = true ]; then
            doctor_reboot "display manager is not active yet"
        else
            doctor_warn "display manager is not active; server will need a graphical login"
        fi
    else
        doctor_warn "systemctl not found; skipping display-manager active check"
    fi

    if [ -S /tmp/.X11-unix/X0 ]; then
        doctor_ok "X socket exists at /tmp/.X11-unix/X0"
    elif [ "$LIGHT_GUI" = true ] || [ "$AUTOLOGIN" = true ]; then
        doctor_reboot "X display :0 is not available yet"
    else
        doctor_warn "X display :0 is not available; start a graphical session before running phantom-server"
    fi

    if have_cmd loginctl && [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
        if loginctl list-sessions --no-legend 2>/dev/null | awk -v u="$TARGET_USER" '$3 == u && $4 == "seat0" && (NF < 6 || $6 == "active") {found=1} END {exit !found}'; then
            doctor_ok "active seat0 session found for $TARGET_USER"
        elif [ "$AUTOLOGIN" = true ]; then
            doctor_reboot "no active seat0 session for $TARGET_USER"
        else
            doctor_warn "no active seat0 session for $TARGET_USER; autostart runs only after graphical login"
        fi

        if loginctl list-sessions --no-legend 2>/dev/null | awk '$4 == "seat0" && (NF < 6 || $6 == "active") && ($3 == "lightdm" || $3 == "gdm" || $3 == "gdm3" || $3 == "sddm") {found=1} END {exit !found}'; then
            if [ "$AUTOLOGIN" = true ]; then
                doctor_fail "display-manager greeter is active on seat0; autologin desktop is locked/backgrounded"
            else
                doctor_warn "display-manager greeter is active on seat0; capture may show the login/lock screen"
            fi
        fi
    fi

    if have_cmd xrandr; then
        _xrandr_out="$(linux_doctor_run_as_user xrandr --query 2>&1 || true)"
        if printf "%s\n" "$_xrandr_out" | grep -Eq ' connected'; then
            doctor_ok "xrandr reports a connected output on DISPLAY=${PHANTOM_DOCTOR_DISPLAY:-:0}"
        else
            _first="$(linux_doctor_first_line "$_xrandr_out")"
            if [ -S /tmp/.X11-unix/X0 ]; then
                doctor_warn "xrandr did not report a connected output: $_first"
            else
                doctor_warn "xrandr cannot query DISPLAY=${PHANTOM_DOCTOR_DISPLAY:-:0} yet: $_first"
            fi
        fi
    else
        doctor_warn "xrandr not found; display-output validation skipped"
    fi
}

linux_doctor_screenlock() {
    if [ "$AUTOLOGIN" != true ] || [ -z "$TARGET_USER" ] || [ "$TARGET_USER" = "root" ]; then
        return 0
    fi

    if [ -n "$USER_HOME" ] && [ -f "$USER_HOME/.config/autostart/phantom-no-screenlock.desktop" ]; then
        doctor_ok "screen-lock prevention autostart exists"
    else
        doctor_warn "screen-lock prevention autostart is missing; rerun install.sh --autologin"
    fi

    if [ -n "$USER_HOME" ] && [ -f "$USER_HOME/.config/autostart/light-locker.desktop" ] \
        && grep -q '^Hidden=true' "$USER_HOME/.config/autostart/light-locker.desktop" 2>/dev/null; then
        doctor_ok "light-locker autostart is disabled for $TARGET_USER"
    elif [ -f /etc/xdg/autostart/light-locker.desktop ]; then
        doctor_warn "light-locker is installed and not disabled for $TARGET_USER"
    fi

    if have_cmd ps && ps -u "$TARGET_USER" -o comm= 2>/dev/null | awk '$1 == "light-locker" || $1 == "xfce4-screensav" || $1 == "gnome-screensav" {found=1} END {exit !found}'; then
        doctor_fail "screen locker is currently running for $TARGET_USER"
    else
        doctor_ok "no screen locker process is running for $TARGET_USER"
    fi
}

linux_doctor_gpu() {
    if linux_has_nvidia_gpu; then
        _gpu_line="$(nvidia-smi -L 2>/dev/null | head -n1 || true)"
        doctor_ok "NVIDIA GPU detected: $_gpu_line"

        if linux_has_dummy_xorg_conf; then
            if [ "$INSTALL_MODE" = "safe" ]; then
                doctor_warn "dummy /etc/X11/xorg.conf is active by --install-mode=safe; NVIDIA zero-copy is disabled"
            else
                doctor_fail "dummy /etc/X11/xorg.conf is active on an NVIDIA VM; it prevents NVIDIA display ownership"
                doctor_fail "remove /etc/X11/xorg.conf or rerun installer after updating; fallback capture will be used until fixed"
            fi
        fi

        if linux_nvidia_display_active; then
            doctor_ok "NVIDIA display_active is Enabled"
        elif [ "$INSTALL_MODE" = "gpu-strict" ]; then
            doctor_fail "NVIDIA GPU exists but display_active is not Enabled"
        else
            doctor_warn "NVIDIA GPU exists but display_active is not Enabled; zero-copy capture may fall back"
        fi

        if have_cmd xrandr; then
            if linux_xorg_has_nvidia_output; then
                doctor_ok "Xorg exposes an NVIDIA display output"
            elif [ "$INSTALL_MODE" = "gpu-strict" ]; then
                doctor_fail "gpu-strict requested but Xorg does not expose a DP/HDMI/DVI NVIDIA output"
            else
                doctor_warn "Xorg does not expose a NVIDIA display output; dummy/CPU fallback may be used"
            fi
        fi
    elif [ "$INSTALL_MODE" = "gpu-strict" ]; then
        doctor_fail "gpu-strict requested but nvidia-smi found no NVIDIA GPU"
    else
        doctor_ok "no NVIDIA GPU detected; CPU/dummy capture path is expected"
    fi
}

linux_doctor_input() {
    if [ -e /dev/uinput ]; then
        doctor_ok "/dev/uinput exists"
    else
        doctor_warn "/dev/uinput is missing; keyboard injection may fall back to XTest"
    fi

    if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
        if id -nG "$TARGET_USER" 2>/dev/null | grep -qw input; then
            doctor_ok "$TARGET_USER is in the input group"
        else
            doctor_reboot "$TARGET_USER is not in the input group yet"
        fi
    else
        doctor_warn "target user is unresolved/root; pass --user=<login> for autologin and input checks"
    fi
}

linux_doctor_autostart() {
    if [ "$NO_AUTOSTART" = true ]; then
        doctor_warn "--no-autostart selected; phantom-server will not start automatically"
        return 0
    fi

    if [ -n "$USER_HOME" ] && [ -f "$USER_HOME/.config/autostart/phantom-server.desktop" ]; then
        doctor_ok "XDG autostart entry exists for $TARGET_USER"
    else
        doctor_warn "XDG autostart entry not found; phantom-server may not start on graphical login"
    fi

    if [ "$AUTOLOGIN" = true ]; then
        if [ -f /etc/lightdm/lightdm.conf.d/60-phantom-autologin.conf ]; then
            doctor_ok "LightDM autologin config exists"
        elif [ -f /etc/gdm3/custom.conf ] && grep -q "AutomaticLogin = $TARGET_USER" /etc/gdm3/custom.conf 2>/dev/null; then
            doctor_ok "GDM autologin config exists"
        else
            doctor_fail "autologin requested but no matching LightDM/GDM autologin config was found"
        fi

        if have_cmd systemctl; then
            if systemctl is-enabled --quiet phantom-autologin-watchdog.timer 2>/dev/null; then
                doctor_ok "autologin watchdog timer is enabled"
            else
                doctor_warn "autologin watchdog timer is not enabled"
            fi
        fi
    fi
}

linux_doctor_capture_probe() {
    _bin="$1"
    if [ ! -x "$_bin" ]; then
        doctor_fail "phantom-server is not executable: $_bin"
        return 0
    fi

    _probe_args="--probe-capture"
    if [ "$INSTALL_MODE" = "gpu-strict" ]; then
        _probe_args="--probe-capture --capture nvfbc --encoder nvenc"
    elif [ "$INSTALL_MODE" = "safe" ]; then
        _probe_args="--probe-capture --capture scrap --encoder openh264"
    fi

    if have_cmd timeout; then
        # shellcheck disable=SC2086 # _probe_args intentionally splits into flags
        _probe_out="$(linux_doctor_run_as_user timeout "$DOCTOR_CAPTURE_TIMEOUT" "$_bin" $_probe_args 2>&1 || true)"
    else
        # shellcheck disable=SC2086 # _probe_args intentionally splits into flags
        _probe_out="$(linux_doctor_run_as_user "$_bin" $_probe_args 2>&1 || true)"
    fi

    if printf "%s\n" "$_probe_out" | grep -q "Capture probe result: pass"; then
        doctor_ok "phantom-server captured and encoded a non-black frame"
        printf "%s\n" "$_probe_out" \
            | grep -E 'Phantom capture probe:|resolved:|gpu_probe:|zero_copy:|frame:|encode:|Capture probe result:' \
            | sed 's/^/    /'
    elif printf "%s\n" "$_probe_out" | grep -q "Capture probe result: mostly-black"; then
        printf "%s\n" "$_probe_out" \
            | grep -E 'Phantom capture probe:|resolved:|gpu_probe:|zero_copy:|frame:|Capture probe result:|mostly black' \
            | sed 's/^/    /'
        if [ "$AUTOLOGIN" = true ] || [ "$LIGHT_GUI" = true ]; then
            doctor_reboot "phantom-server capture probe returned a mostly black frame"
        else
            doctor_fail "phantom-server capture probe returned a mostly black frame"
        fi
    elif printf "%s\n" "$_probe_out" | grep -q "No displays found"; then
        if [ "$AUTOLOGIN" = true ] || [ "$LIGHT_GUI" = true ]; then
            doctor_reboot "phantom-server sees no displays yet"
        else
            doctor_fail "phantom-server sees no capture displays"
        fi
    else
        _first="$(printf "%s\n" "$_probe_out" | grep -E '^(Error:|Caused by:|Capture probe result:)' | head -n1 || true)"
        if [ -z "$_first" ]; then
            _first="$(linux_doctor_first_line "$_probe_out")"
        fi
        if [ "$AUTOLOGIN" = true ] || [ "$LIGHT_GUI" = true ]; then
            doctor_warn "capture probe is not ready yet: $_first"
        else
            doctor_fail "capture probe failed: $_first"
        fi
    fi
}

linux_doctor_runtime() {
    if pgrep -x phantom-server >/dev/null 2>&1; then
        doctor_ok "phantom-server process is running"
        if linux_doctor_port_listening 9901; then
            doctor_ok "browser port 9901 is listening"
        else
            _port_status=$?
            if [ "$_port_status" -eq 2 ]; then
                doctor_warn "cannot check listening ports; ss/lsof not found"
            else
                doctor_warn "phantom-server is running but browser port 9901 is not listening"
            fi
        fi
    elif [ "$DOCTOR_ONLY" = true ] || [ "$DOCTOR_STRICT" = true ]; then
        doctor_fail "phantom-server process is not running"
    elif [ "$AUTOLOGIN" = true ] || [ "$LIGHT_GUI" = true ]; then
        doctor_reboot "phantom-server is not running yet"
    else
        doctor_warn "phantom-server is not running; it starts after graphical login or manual launch"
    fi
}

linux_run_doctor() {
    echo ""
    echo "Running Phantom Linux server doctor..."
    doctor_reset

    if [ "$OS" != "linux" ]; then
        doctor_fail "doctor currently supports Linux server installs only"
        echo ""
        echo "Doctor summary: failures=$DOCTOR_FAILS warnings=$DOCTOR_WARNS reboot_required=$DOCTOR_REBOOT_REQUIRED"
        echo "Doctor result: failed"
        return 1
    fi

    _server_bin="$(linux_phantom_server_bin || true)"
    if [ -n "$_server_bin" ]; then
        doctor_ok "phantom-server found at $_server_bin"
        linux_doctor_runtime_deps "$_server_bin"
    else
        doctor_fail "phantom-server not found on PATH or at $INSTALL_DIR/phantom-server"
    fi

    linux_doctor_display_stack
    linux_doctor_gpu
    linux_doctor_input
    linux_doctor_autostart
    linux_doctor_screenlock
    if [ -n "$_server_bin" ]; then
        linux_doctor_capture_probe "$_server_bin"
    fi
    linux_doctor_runtime

    echo ""
    echo "Doctor summary: failures=$DOCTOR_FAILS warnings=$DOCTOR_WARNS reboot_required=$DOCTOR_REBOOT_REQUIRED"
    if [ "$DOCTOR_REBOOT_REQUIRED" = true ]; then
        echo "Next step: reboot the VM, then rerun the installer with --doctor --doctor-strict"
    fi
    if [ "$DOCTOR_FAILS" -gt 0 ]; then
        echo "Doctor result: failed"
        return 1
    fi
    echo "Doctor result: pass"
    return 0
}

# ===========================================================================
# Main
# ===========================================================================

main() {
    detect_os_arch
    parse_args "$@"
    apply_defaults
    get_target_user

    if [ "$DOCTOR_ONLY" = true ]; then
        linux_run_doctor
        exit $?
    fi

    if [ "$OS" = "linux" ]; then
        linux_install_deps
    fi

    if [ "$INSTALL_SERVER" = true ]; then
        download_and_install "phantom-server"
    fi
    if [ "$INSTALL_CLIENT" = true ]; then
        download_and_install "phantom-client"
    fi

    if [ "$OS" = "linux" ] && [ "$INSTALL_SERVER" = true ]; then
        linux_setup_light_gui_if_requested
        linux_configure_uinput
        if [ "$NO_AUTOSTART" = false ]; then
            linux_install_autostart
        fi
        if [ "$AUTOLOGIN" = true ]; then
            linux_configure_autologin
        fi
        if [ "$SSO" = true ]; then
            linux_install_sso
        fi
        linux_start_server_now
    fi

    DOCTOR_STATUS=0
    if [ "$OS" = "linux" ] && [ "$INSTALL_SERVER" = true ] && [ "$RUN_DOCTOR" = true ]; then
        linux_run_doctor || DOCTOR_STATUS=$?
    fi

    print_post_install_hints
    if [ "$DOCTOR_STATUS" -ne 0 ]; then
        exit "$DOCTOR_STATUS"
    fi
}

main "$@"

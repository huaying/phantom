#!/bin/sh
# Phantom Remote Desktop — install script
# Usage: curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
#
# Installs phantom-server and/or phantom-client to /usr/local/bin.
# On Linux, also installs required runtime libraries.

set -e

REPO="huaying/phantom"
INSTALL_DIR="/usr/local/bin"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

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
    SSO=false
    GOT_ROLE=false

    for _arg in "$@"; do
        case "$_arg" in
            server) INSTALL_SERVER=true; GOT_ROLE=true ;;
            client) INSTALL_CLIENT=true; GOT_ROLE=true ;;
            both)   INSTALL_SERVER=true; INSTALL_CLIENT=true; GOT_ROLE=true ;;
            --autologin) AUTOLOGIN=true ;;
            --sso)  SSO=true ;;
            *) echo "Unknown argument: $_arg"; echo "Usage: $0 [server|client|both] [--autologin] [--sso]"; exit 1 ;;
        esac
    done
}

apply_defaults() {
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
}

# Resolve the invoking non-root user. SUDO_USER is set when install.sh
# is piped through sudo; fall back to $USER.
get_target_user() {
    TARGET_USER="${SUDO_USER:-$USER}"
}

download_and_install() {
    _name="$1"
    _url="${BASE_URL}/${_name}-${OS}-${ARCH}"

    echo "Downloading ${_name}..."
    if have_cmd curl; then
        curl -fsSL "$_url" -o "/tmp/${_name}"
    elif have_cmd wget; then
        wget -qO "/tmp/${_name}" "$_url"
    else
        echo "Error: curl or wget required"; exit 1
    fi

    chmod +x "/tmp/${_name}"

    # Install — use sudo if needed
    if [ -w "$INSTALL_DIR" ]; then
        mv "/tmp/${_name}" "${INSTALL_DIR}/${_name}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "/tmp/${_name}" "${INSTALL_DIR}/${_name}"
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
        sudo apt-get update -qq
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
        sudo pacman -S --needed --noconfirm $_pkgs || true
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
        sudo udevadm trigger /dev/uinput 2>/dev/null || true
        echo "  Wrote $_udev_rule_path"
    else
        echo "  udev rule already in place"
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
    USER_HOME=$(getent passwd "$TARGET_USER" | cut -d: -f6)
    if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
        echo "  ERROR: could not find home directory for $TARGET_USER"
        exit 1
    fi

    linux_autologin_gdm
    linux_autologin_disable_screenlock
    linux_autologin_reset_keyring
    linux_autologin_install_keyring_unlock
    linux_autologin_install_phantom_autostart
    linux_autologin_install_watchdog

    echo ""
    echo "⚠️  Autologin takes effect on next reboot. Security note: the console"
    echo "   will no longer require a password, and the keyring will be stored"
    echo "   unencrypted. This is intended for dedicated remote-access VMs."
}

linux_autologin_gdm() {
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
disable-user-switching=true
EOF
    sudo dconf update 2>/dev/null || true
    echo "  Disabled GNOME screen lock + idle timeout + user switching"
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

linux_autologin_install_phantom_autostart() {
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
    _autostart_dir="$USER_HOME/.config/autostart"
    sudo -u "$TARGET_USER" tee "$_autostart_dir/phantom-server.desktop" > /dev/null <<'EOF'
[Desktop Entry]
Type=Application
Name=Phantom Server
Comment=Remote-desktop server. Edit Exec= below to change transport/encryption/auth.
Exec=sh -c 'pkill -x phantom-server 2>/dev/null; for i in 1 2 3 4 5; do pgrep -x phantom-server >/dev/null 2>&1 || break; sleep 1; done; exec /usr/local/bin/phantom-server --no-encrypt --transport tcp,web'
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF
    echo "  Installed phantom-server autostart entry (edit ~/.config/autostart/phantom-server.desktop to change flags)"
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
}

# ===========================================================================
# Post-install hints
# ===========================================================================

print_post_install_hints() {
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
# Main
# ===========================================================================

main() {
    detect_os_arch
    parse_args "$@"
    apply_defaults
    get_target_user

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
        linux_configure_uinput
        if [ "$AUTOLOGIN" = true ]; then
            linux_configure_autologin
        fi
        if [ "$SSO" = true ]; then
            linux_install_sso
        fi
    fi

    print_post_install_hints
}

main "$@"

#!/bin/sh
# Installs Spinikus on a Linux server with systemd, or upgrades it.
#
#   curl -fsSL https://github.com/cfowlerdev/spinikus/releases/latest/download/install.sh | sudo sh
#
# installs the control plane, which also runs this server's agent and proxy. On another
# server, the dashboard's join command installs only the agent and enrolls it:
#
#   curl -fsSL …/install.sh | sudo sh -s -- --join https://<control plane>:8443 \
#       --token spj_… --ca-fingerprint sha256:…
#
# Running it again upgrades the binary and restarts the service, keeping the config and the
# data. It installs git, Docker (through get.docker.com) and Railpack when they are missing.
#
# Options:
#   --version v<x.y.z>       the release to install (default: the one this script was
#                            published with, or the latest for the copy in the repository)
#   --join <url>             install an agent and enroll it with the control plane at <url>
#   --token <spj_…>          the join token (or SPINIKUS_JOIN_TOKEN)
#   --ca-fingerprint <fp>    the control plane CA's fingerprint, from the join command
#   --http-listen <addr>     where the dashboard and API listen (default 0.0.0.0:8080)
#   --skip-docker            don't install Docker (it must be installed for apps to run)
#   --skip-railpack          don't install Railpack (needed to build repositories without
#                            a Dockerfile)
#   --uninstall              stop and remove the services and the binary, keeping the
#                            config, the data and the containers
#
# Environment: SPINIKUS_DOWNLOAD_URL, a directory URL with spinikus-<target>.tar.gz and
# SHA256SUMS to install from instead of GitHub (for testing builds).

set -eu

REPO="cfowlerdev/spinikus"
# The release this script installs by default. Releases publish it with their own tag here
# (release.yml), so a release candidate's script installs that candidate.
RELEASE="latest"
BIN="/usr/local/bin/spinikus"
CONFIG_DIR="/etc/spinikus"
CONFIG="$CONFIG_DIR/spinikus.toml"
DATA_DIR="/var/lib/spinikus"
USER_NAME="spinikus"
UNIT_DIR="/etc/systemd/system"

main() {
    version="$RELEASE"
    join_url=""
    token="${SPINIKUS_JOIN_TOKEN:-}"
    fingerprint=""
    http_listen="0.0.0.0:8080"
    skip_docker=0
    skip_railpack=0
    uninstall=0

    # Colors only on a terminal.
    if [ -t 2 ]; then
        bold="$(printf '\033[1m')"; yellow="$(printf '\033[33m')"; red="$(printf '\033[31m')"; plain="$(printf '\033[0m')"
    else
        bold=""; yellow=""; red=""; plain=""
    fi
    say() { printf '%s==>%s %s\n' "$bold" "$plain" "$*"; }
    warn() { printf '%swarning:%s %s\n' "$yellow" "$plain" "$*" >&2; }
    die() { printf '%serror:%s %s\n' "$red" "$plain" "$*" >&2; exit 1; }

    need_value() { [ $# -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"; }

    while [ $# -gt 0 ]; do
        case "$1" in
            --version) need_value "$@"; version="$2"; shift 2 ;;
            --join) need_value "$@"; join_url="$2"; shift 2 ;;
            --token) need_value "$@"; token="$2"; shift 2 ;;
            --ca-fingerprint) need_value "$@"; fingerprint="$2"; shift 2 ;;
            --http-listen) need_value "$@"; http_listen="$2"; shift 2 ;;
            --skip-docker) skip_docker=1; shift ;;
            --skip-railpack) skip_railpack=1; shift ;;
            --uninstall) uninstall=1; shift ;;
            -h|--help) echo "See the comments at the top of the script: https://github.com/$REPO/blob/main/install/install.sh"; exit 0 ;;
            *) die "unknown option $1 (see the top of this script)" ;;
        esac
    done

    [ "$(id -u)" -eq 0 ] || die "run it as root, such as with sudo"
    [ "$(uname -s)" = "Linux" ] || die "Spinikus servers run Linux"
    command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ] || die "this script needs systemd"

    if [ "$uninstall" -eq 1 ]; then
        for name in spinikus spinikus-agent; do
            if [ -f "$UNIT_DIR/$name.service" ]; then
                systemctl disable --now "$name" || true
                rm -f "$UNIT_DIR/$name.service"
            fi
        done
        systemctl daemon-reload
        rm -f "$BIN"
        say "Removed the services and $BIN."
        say "Kept $CONFIG_DIR, $DATA_DIR (the database, keys and agent identity) and the containers."
        exit 0
    fi

    # An agent with --join, or when upgrading one; the control plane otherwise.
    if [ -n "$join_url" ] || [ -f "$UNIT_DIR/spinikus-agent.service" ]; then
        [ ! -f "$UNIT_DIR/spinikus.service" ] || die "this server runs the control plane, which has its own agent"
        role="agent"
        unit="spinikus-agent"
        if [ ! -f "$DATA_DIR/agent/agent.json" ]; then
            [ -n "$join_url" ] || die "the agent is not enrolled; pass --join, --token and --ca-fingerprint"
            [ -n "$token" ] || die "--join needs --token (or SPINIKUS_JOIN_TOKEN)"
            [ -n "$fingerprint" ] || die "--join needs --ca-fingerprint"
        fi
    else
        role="server"
        unit="spinikus"
    fi

    case "$(uname -m)" in
        x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
        aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
        *) die "there are no Spinikus builds for $(uname -m)" ;;
    esac

    # --- Packages -------------------------------------------------------------------------

    install_packages() {
        if command -v apt-get >/dev/null 2>&1; then
            DEBIAN_FRONTEND=noninteractive apt-get update -qq
            DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
        elif command -v dnf >/dev/null 2>&1; then
            dnf install -y -q "$@"
        elif command -v yum >/dev/null 2>&1; then
            yum install -y -q "$@"
        elif command -v zypper >/dev/null 2>&1; then
            zypper --non-interactive install "$@"
        elif command -v pacman >/dev/null 2>&1; then
            pacman -S --noconfirm --needed "$@"
        else
            die "install $* and run this again"
        fi
    }

    missing=""
    command -v curl >/dev/null 2>&1 || missing="$missing curl"
    command -v git >/dev/null 2>&1 || missing="$missing git"
    command -v sha256sum >/dev/null 2>&1 || missing="$missing coreutils"
    command -v tar >/dev/null 2>&1 || missing="$missing tar"
    if [ -n "$missing" ]; then
        say "Installing$missing"
        # shellcheck disable=SC2086
        install_packages $missing ca-certificates
    fi

    if ! command -v docker >/dev/null 2>&1; then
        if [ "$skip_docker" -eq 1 ]; then
            warn "Docker is not installed; apps can't run until it is"
        else
            say "Installing Docker (get.docker.com)"
            curl -fsSL https://get.docker.com | sh
        fi
    fi
    if command -v docker >/dev/null 2>&1; then
        systemctl enable --now docker >/dev/null 2>&1 || warn "failed to start Docker; see systemctl status docker"
    fi

    if ! command -v railpack >/dev/null 2>&1 && [ ! -x /usr/local/bin/railpack ]; then
        if [ "$skip_railpack" -eq 1 ]; then
            warn "Railpack is not installed; repositories without a Dockerfile won't build until it is"
        else
            say "Installing Railpack (railpack.com)"
            curl -fsSL https://railpack.com/install.sh | sh -s -- --yes --bin-dir /usr/local/bin \
                || warn "failed to install Railpack; repositories without a Dockerfile won't build until it is"
        fi
    fi

    # --- The binary ------------------------------------------------------------------------

    if [ -n "${SPINIKUS_DOWNLOAD_URL:-}" ]; then
        base="${SPINIKUS_DOWNLOAD_URL%/}"
    elif [ "$version" = "latest" ]; then
        base="https://github.com/$REPO/releases/latest/download"
    else
        case "$version" in v*) ;; *) version="v$version" ;; esac
        base="https://github.com/$REPO/releases/download/$version"
    fi
    archive="spinikus-$target.tar.gz"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    say "Downloading $base/$archive"
    curl -fsSL -o "$work/$archive" "$base/$archive" || die "failed to download $base/$archive"
    curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS" || die "failed to download $base/SHA256SUMS"
    (
        cd "$work"
        grep "  $archive\$" SHA256SUMS > "$archive.sha256" || die "SHA256SUMS has no entry for $archive"
        sha256sum -c --quiet "$archive.sha256" || die "$archive does not match its checksum"
        tar -xzf "$archive" spinikus
    )
    install -m 0755 "$work/spinikus" "$BIN.new"
    mv -f "$BIN.new" "$BIN"
    say "Installed $("$BIN" --version)"

    # --- The user, directories and config -------------------------------------------------

    if ! id "$USER_NAME" >/dev/null 2>&1; then
        nologin="$(command -v nologin || echo /usr/sbin/nologin)"
        useradd --system --user-group --home-dir "$DATA_DIR" --no-create-home --shell "$nologin" "$USER_NAME"
    fi
    # The docker group can run containers, so it is as powerful as root; Spinikus needs it to
    # run apps.
    if getent group docker >/dev/null 2>&1; then
        usermod -aG docker "$USER_NAME"
    fi

    install -d -m 0750 -o root -g "$USER_NAME" "$CONFIG_DIR"
    install -d -m 0750 -o "$USER_NAME" -g "$USER_NAME" "$DATA_DIR"

    if [ ! -f "$CONFIG" ]; then
        say "Writing $CONFIG"
        if [ "$role" = "server" ]; then
            cat > "$CONFIG" <<EOF
# Spinikus instance configuration. Everything else is managed in the dashboard.
data_dir = "$DATA_DIR"

[logging]
level = "info"

[http]
# The dashboard and API, over plain HTTP. Set a dashboard domain in Settings > General to
# serve them over HTTPS through the proxy, then this can be 127.0.0.1:8080.
listen = "$http_listen"

[agents]
# Where agents of other servers connect, over mutual TLS.
listen = "0.0.0.0:8443"

[proxy]
# Where this server's apps are served.
listen = "0.0.0.0:80"
https_listen = "0.0.0.0:443"
EOF
        else
            cat > "$CONFIG" <<EOF
# Spinikus agent configuration. The control plane manages everything else.
data_dir = "$DATA_DIR"

[logging]
level = "info"

[proxy]
# Where this server's apps are served.
listen = "0.0.0.0:80"
https_listen = "0.0.0.0:443"
EOF
        fi
        chown root:"$USER_NAME" "$CONFIG"
        chmod 0640 "$CONFIG"
    fi

    # --- The service ----------------------------------------------------------------------

    if [ "$role" = "server" ]; then
        description="Spinikus control plane"
        command="server run"
    else
        description="Spinikus agent"
        command="agent run"
    fi
    cat > "$UNIT_DIR/$unit.service" <<EOF
[Unit]
Description=$description
Documentation=https://github.com/$REPO
After=network-online.target docker.service
Wants=network-online.target docker.service

[Service]
Type=simple
User=$USER_NAME
Group=$USER_NAME
Environment=SPINIKUS_CONFIG_FILE=$CONFIG
WorkingDirectory=$DATA_DIR
ExecStart=$BIN $command
Restart=on-failure
RestartSec=5s
TimeoutStopSec=30s
# The proxy serves ports 80 and 443.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=full
ProtectHome=yes
PrivateTmp=yes
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload

    as_spinikus() {
        if command -v runuser >/dev/null 2>&1; then
            runuser -u "$USER_NAME" -- "$@"
        else
            su -s /bin/sh "$USER_NAME" -c "$(printf '%s ' "$@")"
        fi
    }

    if [ "$role" = "agent" ] && [ ! -f "$DATA_DIR/agent/agent.json" ]; then
        say "Enrolling the agent with $join_url"
        as_spinikus env SPINIKUS_JOIN_TOKEN="$token" SPINIKUS_CONFIG_FILE="$CONFIG" \
            "$BIN" agent join "$join_url" --ca-fingerprint "$fingerprint"
    fi

    if systemctl is-active --quiet "$unit"; then
        say "Restarting $unit"
        systemctl restart "$unit"
    else
        say "Starting $unit"
        systemctl enable --quiet --now "$unit"
    fi

    if [ "$role" = "agent" ]; then
        say "The agent is running; the server shows as connected in the dashboard."
        say "Logs: journalctl -u $unit -f"
        exit 0
    fi

    # The setup code exists until the first admin is created.
    port="${http_listen##*:}"
    host="$(hostname -I 2>/dev/null | awk '{print $1}')"
    [ -n "$host" ] || host="<this server's address>"
    i=0
    while [ $i -lt 30 ] && [ ! -f "$DATA_DIR/setup-code" ] && ! systemctl is-failed --quiet "$unit"; do
        if [ -f "$DATA_DIR/spinikus.db" ] && [ $i -ge 10 ]; then break; fi
        sleep 1
        i=$((i + 1))
    done
    if systemctl is-failed --quiet "$unit"; then
        die "$unit failed to start; see journalctl -u $unit"
    fi
    echo
    if [ -f "$DATA_DIR/setup-code" ]; then
        say "Spinikus is running. Create the first admin at:"
        echo
        echo "    http://$host:$port/setup?code=$(cat "$DATA_DIR/setup-code")"
        echo
        echo "    The link holds a one-time setup code; keep it to yourself."
    else
        say "Spinikus is running: http://$host:$port/"
    fi
    echo "    Open ports 80 and 443 for apps, $port for the dashboard, and 8443 for other servers' agents."
    echo "    For HTTPS, set a dashboard domain in Settings > General."
    echo "    Logs: journalctl -u $unit -f"
}

# Everything runs from here, once the whole script has been read, so a pipe from curl that
# breaks off early runs nothing.
main "$@"

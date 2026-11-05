#!/usr/bin/env bash
# zetachat: cài hoặc nâng cấp gateway trên một máy Linux có systemd
# (Ubuntu, Debian, container LXC…), chỉ bằng một lệnh:
#
#   curl -fsSL https://raw.githubusercontent.com/Circling5714/zetachat/main/install.sh | sudo bash
#
# Chạy lại lệnh đó là nâng cấp: dữ liệu được giữ, cơ sở dữ liệu được sao lưu
# trước. Tùy chọn đặt sau "bash -s --", ví dụ "| sudo bash -s -- --port 8380":
#
#   --port <số>          cổng web (mặc định 8370)
#   --listen <địa chỉ>   địa chỉ lắng nghe đầy đủ, ví dụ 127.0.0.1:8370
#                        (mặc định 0.0.0.0:<cổng>, mọi máy trong LAN vào được)
#   --version <vX.Y.Z>   bản cần cài (mặc định bản mới nhất)
#   --from-source        build từ mã nguồn thay vì tải bản dựng sẵn
#   --binary <tệp>       cài từ một tệp zetachat-gateway có sẵn
#   --dry-run            chỉ tải và kiểm tra, không thay đổi gì trên máy
#   --uninstall          gỡ dịch vụ và chương trình, giữ dữ liệu
#   --purge              dùng cùng --uninstall: xóa cả dữ liệu (phiên Zalo, tin nhắn)
#   --yes                không hỏi lại

set -euo pipefail

REPO="Circling5714/zetachat"
SERVICE="zetachat-gateway"
BIN="/usr/local/bin/zetachat-gateway"
UNIT="/etc/systemd/system/${SERVICE}.service"
DATA="/var/lib/zetachat"
ACCOUNT="zetachat"

PORT=""
LISTEN=""
VERSION=""
FROM_SOURCE=0
BINARY=""
DRY_RUN=0
UNINSTALL=0
PURGE=0
YES=0

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok() { printf '\033[1;32m ✓\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m !\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[1;31m ✗\033[0m %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Cài hoặc nâng cấp zetachat gateway (Linux có systemd):

  curl -fsSL https://raw.githubusercontent.com/Circling5714/zetachat/main/install.sh | sudo bash

Tùy chọn (đặt sau "bash -s --"):
  --port <số>          cổng web (mặc định 8370)
  --listen <địa chỉ>   địa chỉ lắng nghe đầy đủ, ví dụ 127.0.0.1:8370
  --version <vX.Y.Z>   bản cần cài (mặc định bản mới nhất)
  --from-source        build từ mã nguồn thay vì tải bản dựng sẵn
  --binary <tệp>       cài từ một tệp zetachat-gateway có sẵn
  --dry-run            chỉ tải và kiểm tra, không thay đổi gì trên máy
  --uninstall          gỡ dịch vụ và chương trình, giữ dữ liệu
  --purge              dùng cùng --uninstall: xóa cả dữ liệu
  --yes                không hỏi lại
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="${2:-}"; shift 2 ;;
    --listen) LISTEN="${2:-}"; shift 2 ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --binary) BINARY="${2:-}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --purge) PURGE=1; shift ;;
    --yes | -y) YES=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "Không hiểu tùy chọn: $1 (xem --help)" ;;
  esac
done

if [ -n "$PORT" ]; then
  case "$PORT" in
    '' | *[!0-9]*) die "--port phải là một số" ;;
  esac
  if [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
    die "--port phải từ 1 đến 65535"
  fi
fi
if [ -n "$LISTEN" ] && [ -n "$PORT" ]; then
  die "Chỉ dùng một trong --port và --listen"
fi
if [ -n "$PORT" ]; then
  LISTEN="0.0.0.0:${PORT}"
fi
if [ -n "$LISTEN" ]; then
  case "$LISTEN" in
    *:[0-9]*) ;;
    *) die "--listen cần dạng địa chỉ:cổng, ví dụ 0.0.0.0:8370" ;;
  esac
fi

# Answers yes/no from the terminal even when the script itself comes from a pipe.
confirm() {
  [ "$YES" -eq 1 ] && return 0
  if [ -r /dev/tty ]; then
    local answer
    printf '%s [c/K] ' "$1" >/dev/tty
    read -r answer </dev/tty || return 1
    case "$answer" in c | C | co | có | y | Y | yes) return 0 ;; esac
    return 1
  fi
  die "Cần xác nhận: chạy lại với --yes"
}

need_root() {
  [ "$(id -u)" -eq 0 ] || die "Hãy chạy với quyền root, ví dụ: curl -fsSL … | sudo bash"
}

# --------------------------------------------------------------- checks

[ "$(uname -s)" = "Linux" ] || die "zetachat gateway chỉ chạy trên Linux"
if [ "$DRY_RUN" -eq 0 ]; then
  need_root
  if ! command -v systemctl >/dev/null 2>&1 || ! [ -d /run/systemd/system ]; then
    die "Máy này không dùng systemd; hãy cài theo docs/TRIEN_KHAI.md"
  fi
fi

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64) ARCH="aarch64" ;;
  *)
    ARCH=""
    warn "Chưa có bản dựng sẵn cho $(uname -m): sẽ build từ mã nguồn"
    FROM_SOURCE=1
    ;;
esac

PKG=""
if command -v apt-get >/dev/null 2>&1; then
  PKG="apt"
elif command -v dnf >/dev/null 2>&1; then
  PKG="dnf"
elif command -v yum >/dev/null 2>&1; then
  PKG="yum"
fi

install_packages() {
  [ "$DRY_RUN" -eq 1 ] && die "Thiếu $*; hãy cài trước rồi chạy lại"
  case "$PKG" in
    apt)
      DEBIAN_FRONTEND=noninteractive apt-get update -qq
      DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
      ;;
    dnf) dnf install -y -q "$@" >/dev/null ;;
    yum) yum install -y -q "$@" >/dev/null ;;
    *) die "Thiếu $*; hãy cài bằng trình quản lý gói của máy rồi chạy lại" ;;
  esac
}

need_tools() {
  local missing=()
  command -v curl >/dev/null 2>&1 || missing+=(curl)
  command -v tar >/dev/null 2>&1 || missing+=(tar)
  command -v sha256sum >/dev/null 2>&1 || missing+=(coreutils)
  [ "$PKG" = "apt" ] && ! [ -e /etc/ssl/certs/ca-certificates.crt ] && missing+=(ca-certificates)
  if [ "${#missing[@]}" -gt 0 ]; then
    say "Cài công cụ còn thiếu: ${missing[*]}"
    install_packages "${missing[@]}"
  fi
}

# ------------------------------------------------------------ uninstall

if [ "$UNINSTALL" -eq 1 ]; then
  need_root
  if [ "$PURGE" -eq 1 ]; then
    confirm "Gỡ zetachat VÀ xóa toàn bộ dữ liệu trong $DATA (phiên Zalo, tin nhắn, media)?" ||
      die "Đã hủy"
  else
    confirm "Gỡ zetachat? Dữ liệu trong $DATA được giữ lại." || die "Đã hủy"
  fi
  if systemctl list-unit-files "${SERVICE}.service" >/dev/null 2>&1 &&
    [ -f "$UNIT" ]; then
    systemctl disable --now "$SERVICE" >/dev/null 2>&1 || true
    rm -f "$UNIT"
    systemctl daemon-reload
    ok "Đã gỡ dịch vụ $SERVICE"
  fi
  rm -f "$BIN" "${BIN}.new"
  ok "Đã gỡ $BIN"
  if [ "$PURGE" -eq 1 ]; then
    rm -rf "$DATA"
    if id "$ACCOUNT" >/dev/null 2>&1; then
      userdel "$ACCOUNT" >/dev/null 2>&1 || true
    fi
    ok "Đã xóa dữ liệu và tài khoản hệ thống $ACCOUNT"
  else
    ok "Dữ liệu vẫn ở $DATA (xóa bằng --uninstall --purge)"
  fi
  exit 0
fi

# ------------------------------------------------------ get the program

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

NEW_BIN=""

download() {
  # download <url> <file>
  curl -fsSL --retry 3 --connect-timeout 15 -o "$2" "$1"
}

latest_version() {
  # The tag of the newest release, without asking for jq.
  curl -fsSL --retry 3 --connect-timeout 15 \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null |
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
}

fetch_release() {
  local tag="$VERSION"
  if [ -z "$tag" ]; then
    tag="$(latest_version || true)"
    if [ -z "$tag" ]; then
      warn "Chưa tìm thấy bản phát hành nào trên GitHub"
      return 1
    fi
  fi
  local name="zetachat-gateway-${ARCH}-linux"
  local base="https://github.com/${REPO}/releases/download/${tag}"
  say "Tải ${name} ${tag}"
  download "${base}/${name}.tar.gz" "$WORK/${name}.tar.gz" || {
    warn "Bản ${tag} chưa có tệp cho ${ARCH}"
    return 1
  }
  download "${base}/${name}.tar.gz.sha256" "$WORK/${name}.tar.gz.sha256" || {
    warn "Thiếu tệp kiểm tra SHA-256 của bản ${tag}"
    return 1
  }
  (cd "$WORK" && sha256sum -c --quiet "${name}.tar.gz.sha256") ||
    die "Tệp tải về không khớp SHA-256: dừng lại, không cài gì"
  ok "Đã kiểm tra SHA-256"
  tar -xzf "$WORK/${name}.tar.gz" -C "$WORK"
  NEW_BIN="$WORK/${name}/zetachat-gateway"
  [ -x "$NEW_BIN" ] || die "Gói tải về không có zetachat-gateway"
}

build_from_source() {
  [ "$DRY_RUN" -eq 1 ] && die "--dry-run không build từ mã nguồn"
  say "Build từ mã nguồn (vài phút, cần khoảng 2 GB RAM kể cả swap)"
  case "$PKG" in
    apt) install_packages build-essential pkg-config git curl ca-certificates ;;
    dnf | yum) install_packages gcc gcc-c++ make git curl ;;
    *)
      if ! command -v git >/dev/null 2>&1 || ! command -v cc >/dev/null 2>&1; then
        die "Cần git và trình biên dịch C để build"
      fi
      ;;
  esac
  # Rust lives in a build folder, removed afterwards: nothing is left in ~/.
  local build=/var/cache/zetachat-build
  mkdir -p "$build"
  export RUSTUP_HOME="$build/rustup" CARGO_HOME="$build/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
  if ! [ -x "$CARGO_HOME/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs |
      sh -s -- -y --profile minimal --no-modify-path >/dev/null
  fi
  local ref="${VERSION:-main}"
  rm -rf "$build/src"
  git clone --quiet --depth 1 --branch "$ref" "https://github.com/${REPO}.git" "$build/src"
  (cd "$build/src" && "$CARGO_HOME/bin/cargo" build --release --locked -p zeta-gateway)
  cp "$build/src/target/release/zetachat-gateway" "$WORK/zetachat-gateway"
  NEW_BIN="$WORK/zetachat-gateway"
  rm -rf "$build"
  ok "Đã build xong"
}

if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "Không thấy tệp $BINARY"
  cp "$BINARY" "$WORK/zetachat-gateway"
  chmod 0755 "$WORK/zetachat-gateway"
  NEW_BIN="$WORK/zetachat-gateway"
else
  need_tools
  if [ "$FROM_SOURCE" -eq 0 ] && ! fetch_release; then
    [ "$DRY_RUN" -eq 1 ] && die "Không tải được bản dựng sẵn"
    warn "Chuyển sang build từ mã nguồn"
    FROM_SOURCE=1
  fi
  [ "$FROM_SOURCE" -eq 1 ] && build_from_source
fi

NEW_VERSION="$("$NEW_BIN" --version 2>/dev/null | awk '{print $NF}')" ||
  die "Chương trình vừa tải không chạy được trên máy này"
[ -n "$NEW_VERSION" ] || die "Chương trình vừa tải không chạy được trên máy này"
ok "zetachat-gateway ${NEW_VERSION} chạy được trên máy này"

# --------------------------------------------------------------- plan

FIRST=1
[ -f "$DATA/zetachat.db" ] && FIRST=0
OLD_VERSION=""
[ -x "$BIN" ] && OLD_VERSION="$("$BIN" --version 2>/dev/null | awk '{print $NF}' || true)"

# The address the service listens on: the flag, else what is installed,
# else every interface on 8370.
CURRENT_LISTEN=""
[ -f "$UNIT" ] && CURRENT_LISTEN="$(sed -n 's/^Environment=ZETACHAT_LISTEN=//p' "$UNIT" | tail -n 1)"
TARGET_LISTEN="${LISTEN:-${CURRENT_LISTEN:-0.0.0.0:8370}}"
TARGET_PORT="${TARGET_LISTEN##*:}"
TARGET_HOST="${TARGET_LISTEN%:*}"

if [ "$DRY_RUN" -eq 1 ]; then
  say "Chạy thử (--dry-run): không thay đổi gì trên máy"
  if [ "$FIRST" -eq 1 ]; then
    echo "   - tạo tài khoản hệ thống $ACCOUNT và thư mục dữ liệu $DATA"
  else
    echo "   - sao lưu cơ sở dữ liệu trong $DATA/backups"
  fi
  echo "   - cài $BIN (${OLD_VERSION:-chưa có} → ${NEW_VERSION})"
  echo "   - dịch vụ systemd $SERVICE lắng nghe ${TARGET_LISTEN}"
  [ "$FIRST" -eq 1 ] && echo "   - tạo token cho thiết bị đầu tiên và in link mở zetachat"
  exit 0
fi

# Another program on the port (a gateway started by hand, for one) would
# keep the service from starting.
port_taken_by_other() {
  command -v ss >/dev/null 2>&1 || return 1
  local pids
  pids="$(ss -ltnpH "sport = :${TARGET_PORT}" 2>/dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u)"
  [ -n "$pids" ] || return 1
  local main
  main="$(systemctl show -p MainPID --value "$SERVICE" 2>/dev/null || echo 0)"
  for pid in $pids; do
    [ "$pid" = "$main" ] || return 0
  done
  return 1
}
if port_taken_by_other; then
  die "Cổng ${TARGET_PORT} đang có chương trình khác dùng (có thể là gateway chạy tay). Dừng nó trước, hoặc chọn cổng khác bằng --port"
fi

# ----------------------------------------------------------- install

if ! id "$ACCOUNT" >/dev/null 2>&1; then
  nologin="$(command -v nologin || echo /usr/sbin/nologin)"
  useradd --system --home-dir "$DATA" --no-create-home --shell "$nologin" "$ACCOUNT"
  ok "Đã tạo tài khoản hệ thống $ACCOUNT"
fi
install -d -m 0700 -o "$ACCOUNT" -g "$ACCOUNT" "$DATA"

WAS_RUNNING=0
systemctl is-active --quiet "$SERVICE" 2>/dev/null && WAS_RUNNING=1
if [ "$WAS_RUNNING" -eq 1 ]; then
  say "Dừng dịch vụ để nâng cấp"
  systemctl stop "$SERVICE"
fi

if [ "$FIRST" -eq 0 ]; then
  stamp="$(date +%Y%m%d-%H%M%S)"
  backup="$DATA/backups/$stamp"
  install -d -m 0700 -o "$ACCOUNT" -g "$ACCOUNT" "$DATA/backups" "$backup"
  for f in zetachat.db zetachat.db-wal zetachat.db-shm; do
    [ -f "$DATA/$f" ] && cp -p "$DATA/$f" "$backup/"
  done
  chown -R "$ACCOUNT:$ACCOUNT" "$backup"
  # Keep the five newest backups.
  find "$DATA/backups" -mindepth 1 -maxdepth 1 -type d | sort | head -n -5 | xargs -r rm -rf
  ok "Đã sao lưu cơ sở dữ liệu vào $backup"
fi

install -m 0755 "$NEW_BIN" "${BIN}.new"
mv -f "${BIN}.new" "$BIN"
ok "Đã cài $BIN (${OLD_VERSION:-mới} → ${NEW_VERSION})"

write_unit() {
  cat >"$UNIT" <<EOF
[Unit]
Description=zetachat gateway (one Zalo session for your own devices)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${ACCOUNT}
Group=${ACCOUNT}
# ${DATA}, kept by systemd with mode 0700 for the ${ACCOUNT} user.
StateDirectory=zetachat
StateDirectoryMode=0700
UMask=0077
Environment=ZETACHAT_DATA_DIR=${DATA}
Environment=ZETACHAT_LISTEN=${TARGET_LISTEN}
Environment=RUST_LOG=info
ExecStart=${BIN} serve
Restart=on-failure
RestartSec=5
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF
}

if [ ! -f "$UNIT" ]; then
  write_unit
  ok "Đã tạo dịch vụ systemd $SERVICE"
elif [ -n "$LISTEN" ] && [ "$LISTEN" != "$CURRENT_LISTEN" ]; then
  # Keep the owner's own lines (Environment=…), change only the address.
  if grep -q '^Environment=ZETACHAT_LISTEN=' "$UNIT"; then
    sed -i "s|^Environment=ZETACHAT_LISTEN=.*|Environment=ZETACHAT_LISTEN=${TARGET_LISTEN}|" "$UNIT"
  else
    sed -i "/^\[Service\]/a Environment=ZETACHAT_LISTEN=${TARGET_LISTEN}" "$UNIT"
  fi
  ok "Đã đổi địa chỉ lắng nghe thành ${TARGET_LISTEN}"
fi
systemctl daemon-reload

TOKEN=""
if [ "$FIRST" -eq 1 ]; then
  out="$(runuser -u "$ACCOUNT" -- env ZETACHAT_DATA_DIR="$DATA" "$BIN" device add "Thiết bị đầu tiên" 2>&1)" ||
    die "Không tạo được token cho thiết bị đầu tiên: $out"
  TOKEN="$(printf '%s\n' "$out" | grep -o 'zc_[A-Za-z0-9_-]*' | head -n 1)"
fi

systemctl enable --quiet "$SERVICE"
systemctl restart "$SERVICE"

# Wait until it answers.
probe_host="$TARGET_HOST"
case "$probe_host" in 0.0.0.0 | '' | '[::]' | '::') probe_host="127.0.0.1" ;; esac
healthy=0
for _ in $(seq 1 40); do
  if curl -fsS --max-time 2 "http://${probe_host}:${TARGET_PORT}/api/v1/health" >/dev/null 2>&1; then
    healthy=1
    break
  fi
  sleep 0.5
done
if [ "$healthy" -eq 1 ]; then
  ok "Dịch vụ $SERVICE đang chạy"
else
  warn "Dịch vụ chưa trả lời; xem nhật ký: journalctl -u $SERVICE -n 50"
fi

# --------------------------------------------------------------- done

ip="$TARGET_HOST"
case "$ip" in
  0.0.0.0 | '' | '[::]' | '::')
    ip="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -n 1)"
    [ -n "$ip" ] || ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    [ -n "$ip" ] || ip="<địa chỉ máy này>"
    ;;
esac
url="http://${ip}:${TARGET_PORT}"

echo
if [ "$FIRST" -eq 1 ]; then
  echo "zetachat đã được cài. Bước tiếp theo:"
  echo
  if [ -n "$TOKEN" ]; then
    echo "  1. Mở link này trên máy tính của bạn (token chỉ hiện một lần, hãy giữ kín):"
    echo
    echo "       ${url}/#token=${TOKEN}"
    echo
  else
    echo "  1. Tạo token cho thiết bị (xem dưới) rồi mở ${url}"
  fi
  echo "  2. Trong giao diện, bấm đăng nhập Zalo và quét mã QR bằng app Zalo trên điện thoại."
  echo "     Đừng đăng nhập Zalo PC/Web bằng cùng tài khoản: hai bên sẽ đá phiên nhau."
else
  echo "zetachat đã được nâng cấp lên ${NEW_VERSION}: ${url}"
  echo "Bấm Ctrl+F5 trên các thiết bị để tải giao diện mới."
fi
cat <<EOF

Thêm thiết bị:   sudo -u ${ACCOUNT} ZETACHAT_DATA_DIR=${DATA} zetachat-gateway device add "Tên thiết bị"
Nhật ký:         journalctl -u ${SERVICE} -f
Nâng cấp:        chạy lại lệnh cài đặt
Gỡ cài đặt:      curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | sudo bash -s -- --uninstall

HTTPS và gửi tệp tới 1 GB qua Nginx Proxy Manager: xem docs/TRIEN_KHAI.md, mục 5.
EOF

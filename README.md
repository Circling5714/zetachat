# zetachat

Gateway tự host để dùng **Zalo trên nhiều thiết bị của chính mình**: một phiên đăng nhập duy nhất chạy trên máy chủ ở nhà, các thiết bị khác kết nối vào qua LAN hoặc VPN. Lõi giao thức viết bằng Rust.

> [!WARNING]
> Đây là dự án **không chính thức**, không liên quan tới VNG hay Zalo. Giao thức được tái hiện để tương thích. Dùng client không chính thức có thể vi phạm điều khoản sử dụng của Zalo và khiến tài khoản bị khóa, vì vậy hãy thử bằng tài khoản phụ.

## Vì sao

- Mỗi tài khoản Zalo chỉ có **một suất đăng nhập máy tính** bên cạnh điện thoại; đăng nhập máy thứ hai sẽ đẩy máy trước ra. Giữ một phiên trên máy chủ rồi cho nhiều client nối vào giải quyết việc này.
- Zalo PC (Electron) dùng khoảng **1 GB RAM** trên Windows. Phần nặng (giao thức, mã hóa, lưu trữ) chuyển lên máy chủ, client chỉ còn việc hiển thị.
- Máy chủ luôn bật nên không sót tin; hẹn giờ gửi và tự trả lời vẫn chạy khi máy tính đã tắt.

## Kiến trúc

```text
               Máy chủ Zalo
                     ▲  1 phiên (HTTP + WebSocket)
┌─ Máy chủ ở nhà ────┴─────────────────────┐
│ zetachat gateway (Rust, chạy nền 24/7)   │
│  • zeta-zalo: lõi giao thức Zalo         │
│  • (sau) connector Telegram              │
│  • SQLite: tin nhắn, media, đã đọc       │
│  • API cho client: REST + WebSocket      │
└────────────────────┬─────────────────────┘
                     │  LAN / VPN (không mở ra Internet)
       ┌─────────────┼──────────────┐
  PC Windows      Laptop   Tablet / điện thoại
```

Chi tiết trong [docs/KIEN_TRUC.md](docs/KIEN_TRUC.md). Những gì đã kiểm chứng về giao thức nằm ở [docs/GIAO_THUC.md](docs/GIAO_THUC.md).

## Trạng thái

| Giai đoạn | Nội dung | Trạng thái |
|---|---|---|
| 0 | Chạy thử một phiên Zalo trên máy chủ (bằng ctdozlite) | Phiên chạy được, tin đến và đi về tới máy chủ trong vài giây; đang đo độ bền qua nhiều ngày |
| 1 | `zeta-zalo`: lõi giao thức tách từ ctdozlite | Đang làm. Đã có: đăng nhập QR, lưu phiên, kiểm tra phiên, danh sách bạn bè, gửi tin, WebSocket |
| 2 | Gateway: dịch vụ nền, SQLite, API, token theo thiết bị | Chưa bắt đầu |
| 3 | Client: giao diện web (PWA), bản Windows bằng Tauri | Chưa bắt đầu |
| 4 | Connector Telegram (TDLib) | Dự kiến |

## Cấu trúc

| Đường dẫn | Nội dung |
|---|---|
| `crates/zeta-zalo` | Lõi giao thức: mã hóa, đăng nhập QR, phiên, API, WebSocket |
| `crates/zeta-zalo/examples/zeta.rs` | CLI cho nhà phát triển, để thử với tài khoản thật |
| `docs/` | Kiến trúc và ghi chú giao thức |

## Build và test

Cần Rust stable ([rustup.rs](https://rustup.rs/)); không cần thư viện hệ thống nào khác.

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

CLI thử nghiệm:

```bash
cargo run --example zeta -- login   data/session.json      # quét mã QR bằng app Zalo
cargo run --example zeta -- check   data/session.json      # VALID / EXPIRED / UNREACHABLE
cargo run --example zeta -- friends data/session.json
cargo run --example zeta -- send    data/session.json user <uid> "xin chào"
cargo run --example zeta -- listen  data/session.json 60   # sự kiện thời gian thực
```

## Bảo mật

- File phiên (`session.json`) chứa cookie `zpw_sek` và khóa `zpw_enk`: ai có file này là dùng được tài khoản. `SessionData::save` ghi file với quyền chỉ chủ sở hữu đọc được (trên Unix). Đừng chia sẻ hay commit file này; `.gitignore` đã chặn sẵn.
- Log và thông báo lỗi không chứa URL kèm tham số, cookie hay khóa.
- Gateway sẽ không bao giờ mở ra Internet: truy cập từ xa qua VPN, mỗi thiết bị một token riêng.

## Ghi công và giấy phép

Phát hành theo **MIT OR Apache-2.0** ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).

Phần lớn lõi giao thức được port từ [ctdozlite](https://github.com/ctdoteam1110/ctdozlite) (MIT OR Apache-2.0), xem [NOTICE](NOTICE). Cảm ơn các tác giả ctdozlite đã công bố phần reverse-engineering. zetachat không đóng gói mã JavaScript, hình ảnh hay sticker của Zalo.

---

## English summary

zetachat is a self-hosted gateway that keeps a single Zalo session on a home server and lets your own devices connect to it over LAN or VPN. It works around Zalo's one-desktop-session limit and the ~1 GB of RAM used by the official Electron client. `crates/zeta-zalo` is the Rust protocol core (QR login, session persistence, request encryption, a first set of APIs, the real-time WebSocket), largely ported from ctdozlite with fixes for two bugs described in `docs/GIAO_THUC.md`. Unofficial and not affiliated with VNG or Zalo; using it may violate Zalo's terms of service.

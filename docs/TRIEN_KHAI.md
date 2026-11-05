# Triển khai gateway

Hướng dẫn cho một máy chủ Linux trong LAN, ví dụ container LXC Ubuntu 24.04, đặt sau reverse proxy (Nginx Proxy Manager) và truy cập từ xa qua VPN (WireGuard). Các giá trị trong `<…>` là của riêng bạn.

> [!IMPORTANT]
> Một tài khoản Zalo chỉ có **một suất đăng nhập máy tính**. Khi gateway đang giữ phiên, đừng đăng nhập Zalo PC hay Zalo Web chính thức bằng cùng tài khoản, nếu không hai bên sẽ đá phiên của nhau.

## 1. Build

```bash
sudo apt install -y build-essential pkg-config curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. ~/.cargo/env
git clone https://github.com/Circling5714/zetachat.git && cd zetachat
cargo build --release -p zeta-gateway
sudo install -m 755 target/release/zetachat-gateway /usr/local/bin/
```

## 2. Tài khoản hệ thống và dịch vụ

```bash
sudo useradd --system --home /var/lib/zetachat --shell /usr/sbin/nologin zetachat
sudo install -m 644 deploy/zetachat-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now zetachat-gateway
journalctl -u zetachat-gateway -f
```

Dữ liệu (cơ sở dữ liệu, phiên Zalo và bộ nhớ đệm media) nằm trong `/var/lib/zetachat`, quyền `0700`. Thư mục này chứa thông tin đăng nhập Zalo, cần được sao lưu và bảo vệ như mật khẩu.

Khi bạn đọc tin trên zetachat, gateway báo "đã xem" cho Zalo như Zalo PC: người gửi thấy "Đã xem" và điện thoại hết báo chưa đọc. Muốn đọc mà không báo, thêm `Environment=ZETACHAT_READ_RECEIPTS=false` vào file dịch vụ. Tương tự, khi bạn gõ, người bên kia thấy "đang soạn tin…"; tắt bằng `Environment=ZETACHAT_TYPING=false`.

Ảnh, video và tệp được giữ trong `/var/lib/zetachat/media`, tối đa 2 GB theo mặc định; khi vượt, tệp lâu không dùng nhất bị xóa trước (kiểm tra mỗi giờ). Đổi giới hạn bằng `Environment=ZETACHAT_MEDIA_CACHE_MB=<số MB>` trong file dịch vụ. Thư mục `media` có thể bỏ qua khi sao lưu: tệp nào thiếu sẽ được tải lại từ Zalo khi còn link.

## 3. Thiết bị

Mỗi thiết bị (PC, laptop…) cần một token riêng:

```bash
sudo -u zetachat ZETACHAT_DATA_DIR=/var/lib/zetachat zetachat-gateway device add "Laptop"
sudo -u zetachat ZETACHAT_DATA_DIR=/var/lib/zetachat zetachat-gateway device list
sudo -u zetachat ZETACHAT_DATA_DIR=/var/lib/zetachat zetachat-gateway device revoke <id>
```

Token chỉ hiện một lần. Mất thiết bị thì thu hồi token của nó; phiên Zalo không bị ảnh hưởng.

## 4. Đăng nhập Zalo

Chọn một trong các cách:

- **Qua API** (gateway đang chạy): `POST /api/v1/zalo/login` trả về chuỗi `qr`; vẽ nó thành mã QR rồi quét bằng app Zalo. Xem [API.md](API.md).
- **Trong terminal**: dừng dịch vụ, chạy `sudo -u zetachat ZETACHAT_DATA_DIR=/var/lib/zetachat zetachat-gateway login`, quét mã QR hiện trên màn hình, rồi bật lại dịch vụ.
- **Từ phiên ctdozlite có sẵn**: `zetachat-gateway import-ctdozlite <session.json>`. Phiên do bản đăng nhập trong terminal của ctdozlite tạo ra có thể đã hỏng (xem [GIAO_THUC.md](GIAO_THUC.md), mục 4.1); gateway sẽ báo `rejected` nếu vậy.

Thiết bị hiện trên điện thoại với tên "Ứng dụng: zetachat".

## 5. Reverse proxy và tường lửa

Trong Nginx Proxy Manager, tạo một Proxy Host:

- Domain: `<tên miền nội bộ>`, Scheme `http`, Forward `<IP của máy chủ>`, port `8370`.
- Bật SSL bằng chứng chỉ của bạn.
- Mục *Advanced*, để luồng sự kiện không bị giữ lại:

```nginx
proxy_buffering off;
proxy_read_timeout 1h;
```

Gateway trả tệp tối đa 256 MB; Nginx không giới hạn dung lượng phản hồi nên không cần chỉnh gì thêm cho ảnh và video. Để gửi tệp tới 1 GB như Zalo PC, thêm vào ô *Advanced*:

```nginx
client_max_body_size 1100m;
proxy_request_buffering off;
```

Dòng đầu nâng giới hạn request (Nginx gốc chỉ nhận 1 MB; nếu trước đây bạn đã đặt `100m` thì sửa thành `1100m`). Dòng sau để Nginx chuyển tệp sang gateway ngay trong lúc nhận, thay vì ghi trọn tệp ra đĩa của nó rồi mới chuyển: thanh tiến độ trên giao diện đúng hơn và Nginx không cần chỗ trống cho tệp 1 GB.

Tệp chờ gửi nằm trong `<thư mục dữ liệu>/uploads` cho tới khi lên Zalo xong; gateway xóa chúng ngay sau đó, và xóa phần còn sót lại mỗi lần khởi động.

Chỉ cho Nginx Proxy Manager truy cập cổng của gateway:

```bash
sudo ufw allow from <IP của Nginx Proxy Manager> to any port 8370 proto tcp
```

Nếu reverse proxy chạy trên cùng máy, đặt `ZETACHAT_LISTEN=127.0.0.1:8370` trong file dịch vụ thay vì mở ra LAN. **Không bao giờ mở cổng gateway ra Internet**; khi ở ngoài, hãy truy cập qua VPN.

## 6. Giao diện web

Mở `https://<tên miền nội bộ>/` trên PC.

- **Ghép thiết bị**: lệnh `device add` in ra một link dạng `https://<địa chỉ gateway>/#token=zc_…`. Thay `<địa chỉ gateway>` bằng tên miền của bạn rồi mở link trên đúng thiết bị đó; token nằm sau dấu `#` nên không đi qua máy chủ hay log của proxy. Hoặc dán token vào màn hình "Kết nối".
- **Đăng nhập Zalo**: nếu gateway chưa có phiên, giao diện tự mở hộp "Đăng nhập Zalo"; bấm "Tạo mã QR" rồi quét bằng app Zalo.
- **Cài như ứng dụng**: trong Edge chọn *… → Ứng dụng → Cài đặt trang web này dưới dạng ứng dụng* để có cửa sổ riêng và biểu tượng trên thanh tác vụ.
- **Thông báo**: lần đầu mở một hội thoại, trình duyệt hỏi quyền thông báo; cho phép để nhận tin khi cửa sổ không ở phía trước.

Thông báo, cài ứng dụng và chế độ offline cần HTTPS (hoặc `localhost`), nên hãy truy cập qua Nginx Proxy Manager chứ không phải `http://<IP>:8370`.

## 7. Kiểm tra

```bash
curl https://<tên miền nội bộ>/api/v1/health
curl -H "Authorization: Bearer <token>" https://<tên miền nội bộ>/api/v1/status
```

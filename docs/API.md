# API của gateway

Mọi đường dẫn bắt đầu bằng `/api/v1`. Trừ `/health`, mọi lời gọi đều cần token thiết bị, gửi bằng header `Authorization: Bearer zc_…` hoặc cookie `zetachat_token`. Token được tạo bằng lệnh `zetachat-gateway device add <tên>`.

Lỗi trả về JSON `{ "error": "…" }`: `401` thiếu hoặc sai token, `404` không có hội thoại, `400` dữ liệu không hợp lệ, `502` Zalo từ chối (ví dụ chưa đăng nhập), `503` connector Zalo không chạy.

## Đường dẫn

| Phương thức | Đường dẫn | Mô tả |
|---|---|---|
| GET | `/health` | `{ ok, version }`, không cần token |
| POST | `/session` | Đổi token `{ "token": "zc_…" }` lấy cookie `zetachat_token` (HttpOnly, SameSite=Strict, thêm Secure khi đi qua HTTPS). Giao diện web dùng cách này |
| DELETE | `/session` | Xóa cookie |
| GET | `/status` | `zalo`: trạng thái phiên (`session` là `starting`, `logged_out`, `valid`, `rejected` hoặc `unknown`; `realtime`, `display_name`, `login`, `detail`). `last_event_seq`: số thứ tự sự kiện mới nhất, dùng làm `since` khi mở luồng sự kiện |
| GET | `/threads?limit=&offset=&all=` | Hội thoại, mới nhất trước. `all=true` lấy cả bạn bè chưa từng nhắn tin |
| GET | `/threads/{id}/messages?before=&limit=` | Tin nhắn cũ hơn `seq = before`, cũ nhất trước |
| POST | `/threads/{id}/messages` | Gửi tin `{ "text": "…" }`, trả về tin nhắn đã lưu |
| POST | `/threads/{id}/read` | Đánh dấu đã đọc |
| GET | `/events?since=&limit=` | Nhật ký sự kiện sau `seq = since` |
| GET | `/events/stream` | Luồng sự kiện (server-sent events). Gửi lại header `Last-Event-ID` khi nối lại để không sót sự kiện |
| POST | `/zalo/login` | Bắt đầu đăng nhập QR; trả về `{ "state": "waiting_scan", "qr": "…", "expires_at": … }`. Client vẽ `qr` thành mã QR để quét bằng app Zalo |
| GET | `/zalo/login` | Tiến trình đăng nhập: `idle`, `waiting_scan`, `waiting_confirm`, `failed` |
| GET | `/zalo/login/qr.svg` | Mã QR đang chờ quét dưới dạng ảnh SVG; `404` khi không có |

Mã hội thoại có dạng `zalo:u:<id người dùng>` hoặc `zalo:g:<id nhóm>`. Khi đặt vào URL cần percent-encode nếu thư viện HTTP đòi hỏi.

## Sự kiện

| `event` | `data` |
|---|---|
| `message` | `{ "message": {…}, "thread": {…} }`: tin nhắn mới, đến hay đi |
| `thread_read` | `{ "thread_id": "…" }` |
| `threads_changed` | `{}`: gateway vừa nạp lịch sử (ví dụ tin gần đây khi phiên Zalo khởi động); client tải lại danh sách hội thoại |
| `status` | Trạng thái phiên Zalo, như `/status` |

Mỗi sự kiện có `id` là số thứ tự tăng dần. Client lưu `id` cuối cùng đã nhận; khi mất mạng và nối lại, gửi nó qua `Last-Event-ID` (hoặc `?since=`) để nhận phần còn thiếu.

## Ví dụ

```bash
T=zc_xxxxxxxx
curl -H "Authorization: Bearer $T" https://chat.example.lan/api/v1/threads
curl -H "Authorization: Bearer $T" -H 'content-type: application/json' \
     -d '{"text":"xin chào"}' https://chat.example.lan/api/v1/threads/zalo:u:123/messages
curl -N -H "Authorization: Bearer $T" https://chat.example.lan/api/v1/events/stream
```

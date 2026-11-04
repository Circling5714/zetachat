# API của gateway

Mọi đường dẫn bắt đầu bằng `/api/v1`. Trừ `/health`, mọi lời gọi đều cần token thiết bị, gửi bằng header `Authorization: Bearer zc_…` hoặc cookie `zetachat_token`. Token được tạo bằng lệnh `zetachat-gateway device add <tên>`.

Lỗi trả về JSON `{ "error": "…" }`: `401` thiếu hoặc sai token, `404` không có hội thoại, `400` dữ liệu không hợp lệ, `413` tệp quá lớn để gateway tải, `502` Zalo từ chối (ví dụ chưa đăng nhập) hoặc CDN của Zalo không trả tệp, `503` connector Zalo không chạy.

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
| POST | `/threads/{id}/attachments?name=&caption=&width=&height=` | Gửi ảnh hoặc tệp. Thân request là chính tệp (tối đa 100 MB), `Content-Type` là loại tệp. Ảnh JPEG, PNG, WebP, GIF được gửi dạng ảnh (`width`, `height` là kích thước ảnh nếu client biết); loại khác được gửi dạng tệp. Trả về tin nhắn đã lưu |
| POST | `/threads/{id}/read` | Đánh dấu đã đọc |
| GET | `/events?since=&limit=` | Nhật ký sự kiện sau `seq = since` |
| GET | `/events/stream` | Luồng sự kiện (server-sent events). Gửi lại header `Last-Event-ID` khi nối lại để không sót sự kiện |
| GET | `/media?u=<url>` | Tệp trên CDN của Zalo (ảnh, video, sticker, tin nhắn thoại, tệp đính kèm, ảnh đại diện) qua bộ nhớ đệm của gateway. Chỉ nhận URL thuộc tên miền media của Zalo (`400` với URL khác). Hỗ trợ `Range` để tua video |
| POST | `/zalo/login` | Bắt đầu đăng nhập QR; trả về `{ "state": "waiting_scan", "qr": "…", "expires_at": … }`. Client vẽ `qr` thành mã QR để quét bằng app Zalo |
| GET | `/zalo/login` | Tiến trình đăng nhập: `idle`, `waiting_scan`, `waiting_confirm`, `failed` |
| GET | `/zalo/login/qr.svg` | Mã QR đang chờ quét dưới dạng ảnh SVG; `404` khi không có |

Mã hội thoại có dạng `zalo:u:<id người dùng>` hoặc `zalo:g:<id nhóm>`. Khi đặt vào URL cần percent-encode nếu thư viện HTTP đòi hỏi.

## Tin nhắn và media

Một tin nhắn có `seq`, `thread_id`, `remote_id`, `client_id`, `sender_id`, `sender_name`, `outgoing`, `kind` (`msgType` của Zalo: `webchat`, `chat.photo`, `chat.video.msg`, `chat.sticker`, `share.file`…), `text`, `sent_at` (mili giây) và `media`.

`text` luôn có nội dung đọc được: chữ của tin, hoặc tóm tắt như `[Hình ảnh]`, `[Video]`, `[Tập tin] bao-cao.pdf`, dùng cho danh sách hội thoại và thông báo. `media` là `null` với tin chữ, còn lại là:

| Trường | Ý nghĩa |
|---|---|
| `kind` | `image`, `gif`, `video`, `sticker`, `audio` (tin nhắn thoại), `file` hoặc `link` |
| `url` | Tệp gốc trên CDN của Zalo; với `link` là trang được chia sẻ |
| `thumb` | Ảnh xem trước: ảnh thu nhỏ, ảnh bìa video, ảnh của thẻ liên kết |
| `width`, `height` | Kích thước ảnh hoặc video (0 khi không rõ), để giữ chỗ trước khi tải |
| `name`, `description` | Tên tệp hoặc tiêu đề liên kết; chú thích ảnh hoặc mô tả liên kết |
| `size`, `duration_ms` | Dung lượng (byte) và độ dài video hay tin nhắn thoại, 0 khi không rõ |

Client tải `url` và `thumb` qua `/media?u=…`, không gọi thẳng CDN của Zalo: thiết bị không cần ra Internet, và gateway giữ một bản trên đĩa. Gateway tự tải trước ảnh, sticker, tin nhắn thoại và video (đến 100 MB) của tin mới ngay khi tin đến, nên mở ra là có ngay và vẫn còn khi link của Zalo hết hạn. Tệp đính kèm chỉ được tải khi có người mở. Tệp lớn hơn 256 MB không đi qua gateway (`413`).

Chỉ ảnh, video và âm thanh được trả về để hiển thị trực tiếp; mọi loại khác (HTML, SVG, PDF…) được trả dưới dạng `application/octet-stream` kèm `Content-Disposition: attachment`, để một tệp ai đó gửi vào cuộc trò chuyện không thể chạy như một trang của gateway.

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
curl -H "Authorization: Bearer $T" -H 'content-type: image/jpeg' --data-binary @anh.jpg \
     'https://chat.example.lan/api/v1/threads/zalo:u:123/attachments?name=anh.jpg&width=1280&height=960'
```

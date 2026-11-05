# API của gateway

Mọi đường dẫn bắt đầu bằng `/api/v1`. Trừ `/health`, mọi lời gọi đều cần token thiết bị, gửi bằng header `Authorization: Bearer zc_…` hoặc cookie `zetachat_token`. Token được tạo bằng lệnh `zetachat-gateway device add <tên>`.

Lỗi trả về JSON `{ "error": "…" }`: `401` thiếu hoặc sai token, `404` không có hội thoại, `400` dữ liệu không hợp lệ, `409` không còn hủy được, `413` tệp quá lớn (để gửi hoặc để gateway tải), `429` đang có quá nhiều tệp chờ gửi, `502` Zalo từ chối (ví dụ chưa đăng nhập) hoặc CDN của Zalo không trả tệp, `503` connector Zalo không chạy.

## Đường dẫn

| Phương thức | Đường dẫn | Mô tả |
|---|---|---|
| GET | `/health` | `{ ok, version }`, không cần token |
| POST | `/session` | Đổi token `{ "token": "zc_…" }` lấy cookie `zetachat_token` (HttpOnly, SameSite=Strict, thêm Secure khi đi qua HTTPS). Giao diện web dùng cách này |
| DELETE | `/session` | Xóa cookie |
| GET | `/status` | `zalo`: trạng thái phiên (`session` là `starting`, `logged_out`, `valid`, `rejected` hoặc `unknown`; `realtime`, `display_name`, `uid` và `avatar` của tài khoản, `login`, `detail`). `last_event_seq`: số thứ tự sự kiện mới nhất, dùng làm `since` khi mở luồng sự kiện. `limits`: giới hạn gửi tệp Zalo đang áp dụng (`max_file_bytes`, `max_photo_bytes`, `blocked_extensions`) |
| GET | `/threads?limit=&offset=&all=` | Hội thoại, mới nhất trước. `all=true` lấy cả bạn bè và nhóm chưa từng nhắn tin. Mỗi hội thoại có `members`: số thành viên của nhóm (0 khi chưa biết, và với chat 1-1) |
| GET | `/friends` | Danh sách bạn bè: `[{ "uid", "name", "zalo_name", "avatar", "friend": true, "updated_at" }]`, đồng bộ mỗi khi phiên Zalo khởi động |
| GET | `/people?ids=<uid>,<uid>…` | Tên và ảnh đại diện của người dùng (người gửi tin trong nhóm, thành viên nhóm), tối đa 100 id. Người gateway chưa biết, hoặc đã hơn 3 ngày chưa hỏi lại, được hỏi Zalo trước (50 người mỗi lần). Trả về những người gateway biết, theo thứ tự đã hỏi; người Zalo không trả lời có `name` rỗng |
| GET | `/threads/{id}/members` | Thành viên của nhóm, hỏi Zalo ngay lúc gọi: `{ "total", "members": [{ "uid", "name", "avatar", "friend", "role" }] }`, `role` là `owner` (trưởng nhóm), `admin` (phó nhóm) hoặc `member`; trưởng và phó nhóm đứng trước. Người gateway chưa biết được hỏi tên và ảnh trước. Liệt kê tối đa 300 người. `404` với chat 1-1 |
| GET | `/threads/{id}/shared?type=&before=&limit=` | Những gì đã chia sẻ trong hội thoại, mới nhất trước: `type=media` (ảnh, video, GIF), `file` hoặc `link`. Trả về các tin nhắn mang chúng |
| GET | `/threads/{id}/presence` | Lần cuối người bên kia của chat 1-1 vào Zalo: `{ "visible", "at" }`, `at` tính bằng mili giây, bằng 0 khi họ ẩn hoặc Zalo không cho biết. Gateway hỏi Zalo mỗi người tối đa một lần mỗi phút. `404` với nhóm |
| GET | `/users/find?phone=` | Tài khoản Zalo ứng với số điện thoại: `{ "user": { "uid", "name", "zalo_name", "avatar", "friendship" } }`, hoặc `{ "user": null }` khi không có (hay người đó không cho tìm bằng số). `friendship` là `friend`, `request_sent` (mình đã mời), `request_received` (họ đã mời mình) hoặc `none`. `400` với số quá ngắn hoặc quá dài |
| GET | `/friends/requests` | Lời mời kết bạn đang chờ: `[{ "uid", "display_name", "zalo_name", "avatar", "message", "time" }]` |
| POST | `/friends/requests` | Gửi lời mời kết bạn `{ "uid", "message" }` (lời nhắn tối đa 150 ký tự) |
| POST | `/friends/requests/{uid}/accept` | Đồng ý kết bạn; người đó vào danh sách bạn bè, có hội thoại riêng |
| POST | `/friends/requests/{uid}/reject` | Từ chối lời mời |
| GET | `/stickers/packs` | Các gói nhãn dán trong kho của Zalo, mới nhất trước: `[{ "id", "name", "icon", "count" }]`. Gateway giữ danh sách 12 giờ |
| GET | `/stickers/packs/{id}` | Nhãn dán của một gói: `[{ "id", "cate_id", "type", "url" }]`; `url` là ảnh trên CDN của Zalo, tải qua `/media` |
| GET | `/stickers/search?q=` | Nhãn dán theo từ khóa ("haha", "cảm ơn", "yêu"…), như Zalo gợi ý; cùng dạng như trên |
| POST | `/threads/{id}/stickers` | Gửi nhãn dán `{ "id", "cate_id", "type" }` lấy từ các đường dẫn trên; trả về tin đã lưu (`kind` là `chat.sticker`) |
| GET | `/search?q=&limit=` | Tìm trong nội dung tin nhắn đã lưu, không phân biệt hoa thường và dấu tiếng Việt (`pho` tìm được "Phở"), mới nhất trước: `{ "messages": [...] }`. Tên tệp và chú thích ảnh cũng được tìm, nhãn `[Hình ảnh]`… thì không |
| GET | `/threads/{id}/messages?before=&limit=` | Tin nhắn cũ hơn `seq = before`, cũ nhất trước |
| POST | `/threads/{id}/messages` | Gửi tin `{ "text": "…" }`, trả về tin nhắn đã lưu. Thêm `"reply_to": <seq>` để trả lời một tin: tin mới mang `quote` của tin đó. Trong nhóm, `"mentions": [{ "uid", "pos", "len" }]` nhắc tên: vị trí và độ dài tính theo đơn vị UTF-16 của `text` (như chuỗi JavaScript), `uid` "-1" là cả nhóm. Gateway điều chỉnh vị trí khi cắt khoảng trắng đầu và bỏ những mục nằm ngoài chữ |
| POST | `/threads/{id}/messages/{seq}/recall` | Thu hồi một tin mình đã gửi, với mọi người trong cuộc trò chuyện; trả về tin đã thành `chat.undo`. `409` với tin của người khác, tin chưa được Zalo xác nhận hoặc đã thu hồi |
| POST | `/threads/{id}/messages/{seq}/forward` | Chuyển tiếp một tin `{ "to": ["zalo:u:…", "zalo:g:…"] }` (tối đa 20 hội thoại). Trả về `{ "results": [{ "to", "message" }, { "to", "upload" }, { "to", "error" }] }`, mỗi hội thoại một mục: tin chữ và liên kết đi bằng chức năng chuyển tiếp của Zalo, nhãn dán được gửi lại; ảnh, video, tin nhắn thoại và tệp được gateway gửi lại từ bản nó giữ, thành một lượt gửi tệp (`upload`) để theo dõi. `409` với loại tin chưa chuyển tiếp được (vị trí…) hoặc tin chưa được Zalo xác nhận |
| DELETE | `/threads/{id}/messages/{seq}` | Xóa tin ở phía mình: mất khỏi gateway, và nhờ Zalo, khỏi các thiết bị khác của tài khoản; người khác vẫn thấy tin. Các client nhận sự kiện `message_deleted` |
| POST | `/threads/{id}/messages/{seq}/reactions` | Thả cảm xúc `{ "icon": "/-heart" }`, một trong sáu biểu tượng Zalo cho dùng: `/-strong` 👍, `/-heart` ❤️, `:>` 😆, `:o` 😮, `:-((` 😭, `:-h` 😡. `icon` rỗng gỡ mọi cảm xúc của mình khỏi tin. Trả về tin kèm `reactions` mới; `409` với tin chưa được Zalo xác nhận hoặc đã thu hồi |
| POST | `/threads/{id}/attachments?name=&caption=&width=&height=&as=` | Gửi ảnh hoặc tệp, xem mục "Gửi ảnh và tệp" bên dưới. Trả về `202` với `{ "upload": {…} }` |
| GET | `/uploads` | Các lượt gửi tệp đang chạy và vừa xong (giữ 10 phút) |
| DELETE | `/uploads/{id}` | Hủy một lượt gửi chưa tới bước gửi tin; `409` nếu đã quá muộn |
| POST | `/threads/{id}/read` | Đánh dấu đã đọc. Nếu bật báo đã xem (mặc định), gateway báo cho Zalo về các tin đến chưa đọc: người gửi thấy "Đã xem", điện thoại hết báo chưa đọc |
| POST | `/threads/{id}/typing` | Người dùng đang gõ trong hội thoại này: gateway báo cho Zalo (nếu bật, mặc định có), tối đa mỗi 4 giây một lần. Client gọi lại trong lúc còn gõ |
| GET | `/events?since=&limit=` | Nhật ký sự kiện sau `seq = since` |
| GET | `/events/stream` | Luồng sự kiện (server-sent events). Gửi lại header `Last-Event-ID` khi nối lại để không sót sự kiện |
| GET | `/media?u=<url>` | Tệp trên CDN của Zalo (ảnh, video, sticker, tin nhắn thoại, tệp đính kèm, ảnh đại diện) qua bộ nhớ đệm của gateway. Chỉ nhận URL thuộc tên miền media của Zalo (`400` với URL khác). Hỗ trợ `Range` để tua video |
| POST | `/zalo/login` | Bắt đầu đăng nhập QR; trả về `{ "state": "waiting_scan", "qr": "…", "expires_at": … }`. Client vẽ `qr` thành mã QR để quét bằng app Zalo |
| GET | `/zalo/login` | Tiến trình đăng nhập: `idle`, `waiting_scan`, `waiting_confirm`, `failed` |
| GET | `/zalo/login/qr.svg` | Mã QR đang chờ quét dưới dạng ảnh SVG; `404` khi không có |

Mã hội thoại có dạng `zalo:u:<id người dùng>` hoặc `zalo:g:<id nhóm>`. Khi đặt vào URL cần percent-encode nếu thư viện HTTP đòi hỏi.

## Tin nhắn và media

Một tin nhắn có `seq`, `thread_id`, `remote_id`, `client_id`, `sender_id`, `sender_name`, `outgoing`, `kind` (`msgType` của Zalo: `webchat`, `chat.photo`, `chat.video.msg`, `chat.sticker`, `share.file`…), `text`, `sent_at` (mili giây), `media`, `quote`, `mentions` (người được nhắc tên: `[{ "uid", "pos", "len" }]`, vị trí theo UTF-16) và `status`: với tin của chính mình, 0 là đã gửi, 1 là đã tới máy người nhận, 2 là đã được xem.

`quote` là tin được trả lời (`null` nếu không phải tin trả lời): `{ "sender_name", "text", "remote_id" }`. `reactions` là cảm xúc dưới tin, nhiều nhất trước: `[{ "icon": "/-heart", "emoji": "❤️", "count": 3, "mine": true }]`; mỗi người thả được nhiều lần, `count` đếm tất cả, `mine` cho biết tài khoản này có thả biểu tượng đó. Tin bị người gửi thu hồi có `kind` là `chat.undo`, `text` là "Tin nhắn đã được thu hồi", không còn `media` hay `quote`; gateway xóa hẳn nội dung gốc khỏi kho.

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

## Gửi ảnh và tệp

Thân request là chính tệp, `Content-Type` là loại tệp. Gateway ghi tệp xuống đĩa trong lúc nhận (không giữ cả tệp trong RAM), trả `202` ngay khi nhận đủ, rồi tự đưa tệp lên Zalo ở nền: từng tệp một, theo thứ tự, mỗi tệp chia thành nhiều phần 3 MB gửi song song. Thiết bị có thể đóng trang sau khi nhận `202`; tệp vẫn được gửi tiếp.

Giới hạn là của Zalo; gateway đọc chúng từ máy chủ Zalo mỗi khi phiên khởi động và báo lại trong `/status` → `limits`. Tệp tối đa 1 GB. Zalo không nhận tệp `.exe`, `.bat`, `.cmd`, `.msi`… (`400`; hãy nén thành `.zip`). Ảnh JPEG, PNG, WebP, GIF đến 100 MB được gửi dạng ảnh (`width`, `height` là kích thước ảnh nếu client biết); ảnh lớn hơn, ảnh gửi kèm `as=file` và mọi loại khác được gửi dạng tệp.

Một lượt gửi (`upload`) có `id`, `thread_id`, `name`, `size`, `kind` (`photo` hoặc `file`), `sent` (số byte Zalo đã nhận), `error`, `message` (tin đã gửi, khi xong) và `state`:

| `state` | Ý nghĩa |
|---|---|
| `queued` | Chờ các tệp trước gửi xong |
| `sending` | Đang gửi lên Zalo; khi `sent` bằng `size`, Zalo đang xử lý tệp |
| `finishing` | Zalo đã có tệp, đang gửi tin; không hủy được nữa |
| `done` | Đã gửi; tin cũng tới qua sự kiện `message` |
| `failed` | Không gửi được, `error` nói vì sao |
| `cancelled` | Đã hủy |

Lượt gửi không sống qua lần khởi động lại gateway: tệp chưa tới Zalo phải gửi lại.

## Sự kiện

| `event` | `data` |
|---|---|
| `message` | `{ "message": {…}, "thread": {…} }`: tin nhắn mới, đến hay đi |
| `message_updated` | `{ "message": {…}, "thread": {…} }`: một tin đã lưu thay đổi (bị thu hồi, có cảm xúc mới); client thay tin có cùng `seq` |
| `message_deleted` | `{ "thread_id", "seq", "thread" }`: một tin đã bị xóa ở phía mình; client bỏ tin có `seq` đó |
| `thread_read` | `{ "thread_id", "unread" }`: hội thoại đã được đọc, ở đây hoặc trên thiết bị khác của tài khoản (ví dụ điện thoại); `unread` là số tin còn chưa đọc (thường là 0) |
| `threads_changed` | `{}`: gateway vừa nạp lịch sử (ví dụ tin gần đây khi phiên Zalo khởi động); client tải lại danh sách hội thoại |
| `status` | Trạng thái phiên Zalo, như `/status` |
| `upload` | `{ "upload": {…} }`: tiến độ một lượt gửi tệp, vài lần mỗi giây khi đang gửi |
| `typing` | `{ "thread_id", "user_id", "name" }`: ai đó đang soạn tin; Zalo nhắc lại vài giây một lần trong lúc họ còn gõ, client tự tắt sau khoảng 6 giây |

Mỗi sự kiện có `id` là số thứ tự tăng dần. Client lưu `id` cuối cùng đã nhận; khi mất mạng và nối lại, gửi nó qua `Last-Event-ID` (hoặc `?since=`) để nhận phần còn thiếu. Riêng `upload` và `typing` không vào nhật ký và không có `id`: client đang mất kết nối sẽ bỏ lỡ chúng; khi nối lại, hỏi `/uploads` để biết các lượt gửi tệp.

## Ví dụ

```bash
T=zc_xxxxxxxx
curl -H "Authorization: Bearer $T" https://chat.example.lan/api/v1/threads
curl -H "Authorization: Bearer $T" -H 'content-type: application/json' \
     -d '{"text":"xin chào"}' https://chat.example.lan/api/v1/threads/zalo:u:123/messages
curl -N -H "Authorization: Bearer $T" https://chat.example.lan/api/v1/events/stream
curl -H "Authorization: Bearer $T" -H 'content-type: image/jpeg' --data-binary @anh.jpg \
     'https://chat.example.lan/api/v1/threads/zalo:u:123/attachments?name=anh.jpg&width=1280&height=960'
# Tệp lớn: -T đọc dần từ đĩa thay vì nạp cả tệp vào RAM
curl -X POST -T video.mp4 -H "Authorization: Bearer $T" -H 'content-type: video/mp4' \
     'https://chat.example.lan/api/v1/threads/zalo:u:123/attachments?name=video.mp4'
curl -H "Authorization: Bearer $T" https://chat.example.lan/api/v1/uploads
```

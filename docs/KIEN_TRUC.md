# Kiến trúc zetachat

## Mục tiêu

- Dùng một tài khoản Zalo trên nhiều thiết bị của mình mà không bị đá phiên.
- Client nhẹ hơn nhiều so với Zalo PC.
- Thêm Telegram khi phần Zalo đã ổn định.

## Thành phần

1. **zeta-zalo**: thư viện giao thức (`crates/zeta-zalo`).
2. **Gateway**: dịch vụ chạy nền 24/7 trên máy chủ ở nhà, ví dụ một container LXC Ubuntu 24.04.
   - Giữ **phiên Zalo duy nhất** và kết nối WebSocket.
   - Lưu tin nhắn, media và trạng thái đã đọc vào SQLite. Đây là nguồn dữ liệu duy nhất.
   - Giữ bộ nhớ đệm media trên đĩa (`<thư mục dữ liệu>/media`). Ảnh, sticker, tin nhắn thoại và video của tin mới được tải về ngay khi tin đến. Tệp cũ ít dùng nhất bị xóa khi vượt dung lượng cho phép.
   - Cung cấp API REST và WebSocket cho client. Mỗi sự kiện có số thứ tự, nên client mất mạng khi nối lại sẽ không sót tin.
   - Chạy tác vụ nền: hẹn giờ gửi, tự trả lời.
3. **Client**: giao diện web (PWA) do gateway phục vụ; bản Windows được bọc bằng Tauri để có biểu tượng khay hệ thống và thông báo. Chọn web vì một mã nguồn chạy được trên mọi thiết bị, và bộ gõ tiếng Việt do trình duyệt xử lý ổn định.
4. **Connector Telegram** (làm sau) qua TDLib, là API chính thức của Telegram.

## Nguyên tắc

- **Chỉ gateway nói chuyện với Zalo**; client chỉ nói chuyện với gateway, kể cả để lấy ảnh và video. Gateway chỉ tải từ các tên miền media của Zalo, nên một tin nhắn không thể khiến nó gọi tới địa chỉ khác, ví dụ máy trong LAN.
- **Client đọc dữ liệu từ kho của gateway**, gateway không chuyển tiếp từng thao tác của client lên Zalo. Nhờ vậy số request ít, nguy cơ bị giới hạn tần suất hay khóa tài khoản thấp.
- **Không mở gateway ra Internet.** Truy cập từ xa qua VPN (WireGuard), HTTPS qua reverse proxy trong LAN, mỗi thiết bị một token và có thể thu hồi.
- **Phiên là bí mật**: file quyền 600, không ghi log, không commit.
- **Một phiên Zalo chiếm một suất máy tính**: khi đã dùng gateway thì không đăng nhập song song Zalo PC hay Zalo Web chính thức bằng cùng tài khoản.

## Giới hạn đã biết

- Gọi thoại và video chưa nằm trong phạm vi, vì âm thanh và hình ảnh phải đi qua gateway mà vẫn cần độ trễ thấp.
- Khi phiên hết hạn, gateway sẽ hiển thị mã QR mới trên client để quét lại.
- Dùng client không chính thức có thể vi phạm điều khoản của Zalo.

# Ghi chú giao thức Zalo Web/PC

Nội dung dưới đây hoặc đã được **kiểm chứng trực tiếp** với máy chủ Zalo (tháng 11/2026) bằng một tài khoản thử nghiệm, hoặc được port từ ghi chú reverse-engineering của ctdozlite (khi đó có ghi rõ). Tài liệu không chứa dữ liệu của tài khoản nào.

## 1. Tham số chung

- Mọi request có `zpw_ver=694` và `zpw_type=30` (client PC/Web).
- Header `Origin: https://chat.zalo.me`, `Referer: https://chat.zalo.me/` và User-Agent của Zalo PC (Electron).

## 2. Mã hóa

- `signkey = md5("zsecure" + tag + các giá trị tham số, sắp theo tên khóa)`. `tag` là tên lệnh viết thường, ví dụ `reqqr`, `getserverinfo`, `getlogininfo`.
- **ParamCipher v2**, dùng khi chưa có khóa phiên: `zcid = hex_upper(AES-256-CBC(khóa tĩnh, "{type},{imei},{thời điểm}"))`, `zcid_ext` là chuỗi hex ngẫu nhiên, khóa 32 ký tự ghép từ `md5(zcid_ext)` và `zcid`. Máy chủ mã hóa phản hồi bằng chính khóa này.
- Sau khi đăng nhập: `params` và `data` được mã hóa bằng **AES-CBC, IV toàn số 0, đệm PKCS7** với khóa `zpw_enk` (base64 của 16 hoặc 32 byte).

## 3. Đăng nhập bằng mã QR (đã kiểm chứng)

1. `GET {auth}/api/login/getServerInfo` (có `signkey`) trả về bảng host.
2. `GET {auth}/api/login/reqqr` với `self_g=1` trả về `qr_cnt` (nội dung mã QR), `chk_wait_scan`, `chk_wait_cfirm` và `qr_timeout` (180 giây).
3. Long-poll `chk_wait_scan&lang=vi`: **HTTP 408 nghĩa là còn chờ**. Khi điện thoại quét, phản hồi có `display_name` và `avatar`.
4. Poll `chk_wait_cfirm`: lỗi `18012` nghĩa là đã quét nhưng chưa bấm "Đăng nhập". Khi người dùng bấm, phản hồi có:
   - `Set-Cookie: zpw_sek=…; Domain=zaloapp.com`: **cookie phiên**, dài khoảng 100 ký tự;
   - `data.zpw_sek` (cùng giá trị), `data.uid`, `data.zpw_kt`, `data.dkey`, `data.sync_msg_info` và vài trường khác.
5. `GET {auth}/api/login/getLoginInfo` (ParamCipher v2) trả về `zpw_enk`, `uid`, `send2me_id`, `zpw_ws`, `zpw_service_map_v3`, `zpw_chts_new`, `zpw_ctrs_new`, `zpw_wpsk` và các trường khác.
6. Kiểm tra lại phiên bằng `me-v2` (mục 5).

Trên màn hình xác nhận của điện thoại, "Ứng dụng" là `computer_name` mà client gửi lên, "Thiết bị" được suy ra từ User-Agent (`Chrome/122` hiện thành "Chrome 12 - Windows 10"), còn "Địa điểm" được suy ra từ địa chỉ IP.

## 4. Hai cái bẫy đã gặp

### 4.1. `zpw_wpsk` không phải `zpw_sek`

`getLoginInfo` **không** trả về `zpw_sek`; nó trả về `zpw_wpsk` (khoảng 87 ký tự), một token khác. Bản đăng nhập trong terminal của ctdozlite dò lần lượt các tên `zpw_sek`, `zpw_kt`, `zpw_wpsk`, `token`, `session_key` trong phản hồi này, vớ phải `zpw_wpsk` và ghi đè cookie phiên đúng. Từ đó mọi request đều lỗi `600 zpw_sek bị thiếu hoặc không đúng`.

**zeta-zalo chỉ lấy `zpw_sek` từ bước xác nhận QR** (`login::session_cookie_from_confirm`).

### 4.2. Phản hồi rỗng không phải là thành công

Khi không dùng được phiên, đôi khi Zalo trả **HTTP 200 với thân rỗng** thay vì báo lỗi. ctdozlite coi thân rỗng là `error_code 0`, nên bước kiểm tra phiên của nó báo "hợp lệ" trong khi phiên đang bị từ chối.

**zeta-zalo coi thân rỗng là lỗi** (`Error::EmptyResponse`). `me-v2` chỉ được tính là thành công khi phản hồi có hồ sơ người dùng.

## 5. Cookie và host

- Gửi `zpw_sek` dưới dạng **cookie** là đủ, trên cả host `*.chat.zalo.me` lẫn `*.zaloapp.com`. Chỉ gửi header `zpw_sek` mà không có cookie thì nhận về thân rỗng.
- `me-v2`: `{profile}/api/social/profile/me-v2?…&params=<mã hóa {avatar_size, imei}>&os=8` trả về `data.profile` với `userId`, `displayName`, `avatar`.
- `getfriends` trả **404 trên cả hai host `tt-friend-wpa`** và chạy được trên `tt-profile-wpa.chat.zalo.me`. zeta-zalo thử host friend trước rồi chuyển sang host profile.
- Mã lỗi `600` và `-102`: phiên bị từ chối, cần đăng nhập lại. `429`: gọi quá dày, nhất là `getfriends`.

## 6. WebSocket thời gian thực (đã kiểm chứng với gateway)

- Địa chỉ `wss://ws{N}-msg.chat.zalo.me/?zpw_ver=…&zpw_type=…&t=<ms>`, kèm cookie `zpw_sek` (không cần header `zpw_sek`).
- Đã kiểm chứng tháng 11/2026: xác thực `cmd 1` thành công, sau đó máy chủ trả các hàng đợi tin offline (`510`, `511`, `515`, `517`, `518`, `603`, `610`, `611`); tin vừa gửi bằng HTTP được gửi lại qua `cmd 501` với cùng `cliMsgId`; ping `cmd 2` được trả lời.
- Khung: `u8 version | u16 LE cmd | u8 subCmd | JSON`. Máy chủ gửi trước `cmd 1` kèm khóa AES-GCM. Trường `encrypt`: 0 không mã hóa, 1 base64 + nén, 2 AES-GCM + nén, 3 AES-GCM (`iv 16 | aad 16 | ciphertext+tag`).
- Sau khi xác thực, client kéo các hàng đợi có trong `ctrl_qCmds` / `qCmds`. Kéo hàng đợi ngoài danh sách sẽ bị đóng kết nối. Ping mỗi 15 giây.
- `cmd` của tin nhắn: 501/521 (máy chủ đẩy), 510/511 (client kéo), 533 (có tin mới) rồi 570 (lấy tin). Điều khiển: 601–604.

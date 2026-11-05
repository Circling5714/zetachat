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

## 7. Nội dung tin nhắn và media (đã kiểm chứng)

Trường `content` của tin nhắn là chuỗi với tin chữ (`msgType` `webchat`). Với các loại khác nó là object; `params` bên trong là **một chuỗi JSON** cần parse thêm lần nữa.

| `msgType` | Nội dung |
|---|---|
| `chat.photo` | `href`, `thumb` (đều là URL ảnh), `title` (chú thích), `params`: `width`, `height`, `hd` (URL), `thumb_width`, `thumb_height`. Đã gặp `hd` trùng `href`, và `thumb` cùng dung lượng với ảnh gốc |
| `chat.video.msg` | `href` (video), `thumb` (ảnh bìa), `params`: `video_width`, `video_height`, `video_rotation`, `duration` (ms), `fileSize` |
| `chat.sticker` | Chỉ có `id`, `catId`, `type`. Ảnh: nếu `catId = 0` và `id` từ 1 đến 30 thì là `https://stc-chat.zdn.vn/images/stickers/default/thumb/{id}.png`, còn lại `https://api.zaloapp.com/api/emoticon/sticker/webpc?eid={id}&size=130` (quy tắc của ctdozlite, đã kiểm chứng với một sticker `catId` khác 0) |
| `share.file` | `href`, `title` (tên tệp), `params.fileSize` (theo ctdozlite) |
| `chat.recommended`, `chat.link` | Thẻ liên kết: `href`, `title`, `description`, `thumb` |
| `chat.webcontent` | Thẻ đặc biệt; câu thay thế nằm ở `params.customMsg.msg` |
| `chat.undo` | Thu hồi tin: `globalMsgId` và `cliMsgId` của tin bị thu hồi, `deleteMsg`, `srcId`, `destId`. Định tuyến như tin thường (cùng hội thoại với tin gốc) |

Tin trả lời có thêm object `quote` ở cấp tin nhắn: `fromD` (tên người gửi tin gốc), `msg` (chữ của tin gốc), `attach` (chuỗi JSON khi tin gốc là tệp đính kèm), `globalMsgId`, `cliMsgId`, `ownerId`, `ts` (theo zca-js; chưa gặp trên tài khoản thử).

CDN media trả tệp **không cần cookie phiên**, chỉ với User-Agent và `Referer: https://chat.zalo.me/`:

- Ảnh ở `photo-stal-*.zdn.vn`, gửi kèm `Content-Type: image/jpg` (không chuẩn; đúng phải là `image/jpeg`). Host ảnh trả `200` cho cả request có `Range`.
- Video ở `video-stal-*.dlmd.me`, hỗ trợ `Range` (`206`). Ảnh bìa video ở `photo-stal-*.zdn.vn`.
- Sticker ở `api.zaloapp.com` (`image/png`). Ảnh đại diện ở `*.zadn.vn`.

Gateway chỉ tải từ các tên miền `zdn.vn`, `zadn.vn`, `dlmd.me`, `zaloapp.com`, `zalo.me` và tên miền con của chúng (`zeta_zalo::media::is_media_url`); chuyển hướng sang tên miền khác bị chặn.

## 8. Gửi ảnh và tệp (đã kiểm chứng)

Cả hai đều tải lên host `file` (`tt-files-wpa.zaloapp.com`) theo từng phần: `POST {file}/api/{message|group}/{loại}/upload?…&params=<mã hóa>&type=<2 cho người, 11 cho nhóm>`, thân `multipart/form-data` với trường `chunkContent`. `params` gồm `totalChunk`, `chunkId` (từ 1), `fileName`, `totalSize`, `clientId` (giữ nguyên cho mọi phần), `imei`, `isE2EE: 0`, `toid` hoặc `grid`; với ảnh thêm `jxl: 0`.

- **Ảnh**: loại `photo_original`. Phần cuối trả về `photoId`, `normalUrl`, `hdUrl`, `thumbUrl`; sau đó `POST {file}/api/…/photo_original/send` với `photoId`, `clientId`, `desc` (chú thích), `width`, `height` và các URL. Ảnh nằm trên `*-zpcloud.zdn.vn`.
- **Tệp**: loại `asyncfile`. Phần cuối trả về `fileId`. Khi lưu xong, Zalo báo qua WebSocket: `cmd 601`, `data.controls[].content` có `act_type: "file_done"`, `fileId` và `data.url`; sự kiện này có thể tới trước cả phản hồi HTTP của phần cuối. Sau đó `POST {file}/api/…/asyncfile/msg` với `fileId`, `fileUrl`, `checksum` (MD5), `fileName`, `totalSize`, `clientId`, `fType: 1`. Tệp nằm trên `*-zfcloud.zdn.vn`.

Zalo gửi lại tin vừa gửi qua WebSocket với cùng `clientId` (`cliMsgId`), nên gateway không lưu trùng.

**Giới hạn và tệp lớn.** Giới hạn nằm trong `getServerInfo` (gọi được không cần phiên): `data.setttings.features.sharefile` (Zalo viết sai thành `setttings`). Tháng 11/2026: `chunk_size_file` 3 MiB, `max_size_share_file_v3` 1024 (MB), `max_size_original_photo` 100 (MB), `restricted_ext_file` gồm `exe`, `cmd`, `bat`, `com`, `lnk`, `vbs`, `msi`, `vb`, `ws`, `wsf`, `scf`, `scr`, `pif`, `chm`. Gateway đọc lại mỗi khi phiên khởi động (lệnh `zeta limits <session.json>` in phần này).

Các phần không cần gửi theo thứ tự: gateway gửi 3 phần cùng lúc, chừa phần cuối gửi riêng sau khi mọi phần khác đã xong; phần cuối trả `fileId` thật (theo zca-js, các phần trước trả `fileId: -1`). Đo trên tài khoản thử (đường lên khoảng 20 MB/s): 20 MB xong trong 1,6 giây, 300 MB trong 18 giây, 1 GB trong 44 giây, trong đó Zalo mất 2–7 giây xử lý trước khi báo `file_done`. Tệp vài trăm MB nằm trên `*.dlfl.vn` (ví dụ `bfg30.dlfl.vn`), cũng không cần cookie và nhận `Range`. `retry_upload_file_configs.upload_file_timeout_replay_last_chunk` (`[10, 35, 40, 45, 50, 60]` giây) cho thấy Zalo PC gửi lại phần cuối khi chờ lâu; gateway làm tương tự khi `file_done` không tới (tối đa 2 lần).

## 8b. Cảm xúc (đã kiểm chứng)

Thả cảm xúc: `POST {reaction}/api/message/reaction` (nhóm: `/api/group/reaction`), host `reaction` là `reaction.api.zaloapp.com`. `params` mã hóa gồm `react_list: [{ message, clientId }]`, trong đó `message` là chuỗi JSON `{ rMsg: [{ gMsgID, cMsgID, msgType: 1 }], rIcon, rType, source: 6 }`, và `toid` (nhóm: `grid` cùng `imei`). Sáu biểu tượng Zalo cho dùng và `rType` của chúng nằm trong `getServerInfo` → `setttings.features.reaction_setting.reactionMsgInfo`: `/-strong` 3, `/-heart` 5, `:>` 0, `:o` 32, `:-((` 2, `:-h` 20. `rIcon` rỗng với `rType` -1 là gỡ. Phản hồi là `{ msgIds: "[<id>]" }`, mảng nằm trong một chuỗi JSON.

Nhận cảm xúc: `cmd 612` khi vừa xảy ra; `610` và `611` (sub 1) chở những cái bị lỡ, mỗi lần kết nối. Hai danh sách `reacts` (1-1) và `reactGroups` có các trường như tin nhắn (`msgId`, `actionId`, `cliMsgId`, `uidFrom`, `idTo`, `dName`, `ts`), còn `content` là chuỗi JSON chứa `rMsg`, `rIcon`, `rType`, `msgSender`. Một cảm xúc có thể áp vào nhiều tin cùng lúc (đã thấy `rMsg` có 3 phần tử). Mỗi lần thả là một hành động, nên số đếm là tổng số lần thả; `rIcon` rỗng xóa mọi lần thả trước của người đó. Zalo gửi lại cảm xúc của chính tài khoản qua WebSocket; gateway nhận ra bản sao nhờ `msgId` hoặc `cliMsgId` (bằng `clientId` đã gửi) nên không đếm hai lần.

## 8c. Đang soạn tin (gửi đã kiểm chứng, nhận theo zca-js)

Báo mình đang gõ: `POST {chat}/api/message/typing` với `toid`, `destType: 3`, `imei`; nhóm: `POST {group}/api/group/typing` với `grid`, `imei`. Zalo trả lời không lỗi (thử vào Cloud). Người khác gõ tới qua `cmd 602`: `data.actions[]` có `act_type: "typing"`, `act` là `typing` (1-1) hoặc `gtyping` (nhóm), và `data` chứa `uid`, `ts`, `isPC`, `gid` (nhóm). Theo zca-js `data` là chuỗi JSON bị bỏ dấu ngoặc nhọn; gateway nhận cả ba dạng (thiếu ngoặc, đủ ngoặc, object) và ghi hình dạng khung vào log debug để kiểm chứng khi có người gõ thật.

## 8d. Trả lời và thu hồi (đã kiểm chứng)

Trả lời một tin: `POST {chat}/api/message/quote` (nhóm: `{group}/api/group/quote`) với chữ của tin mới và `qmsgOwner` (người gửi tin được trả lời; tin của mình thì là uid của mình), `qmsgId`, `qmsgCliId`, `qmsgType` (số: 1 chữ, 32 ảnh, 36 sticker, 44 video, 46 tệp…), `qmsgTs`, `qmsg` (chữ được trích, rỗng với media), `qmsgTTL`, `ttl: 0`; 1-1 thêm `imei`, nhóm thêm `visibility: 0` và `qmsgAttach` (chuỗi JSON của media được trích, với `thumbUrl`, `oriUrl`, `normalUrl`). Đã thử trả lời một tin tệp trong Cloud: Zalo nhận, bản sao gửi về có cùng `cliMsgId` nên không bị lưu trùng.

Thu hồi tin của mình: `POST {chat}/api/message/undo` (nhóm: `{group}/api/group/undomsg` thêm `visibility: 0`, `imei`) với `msgId`, `clientId` mới và `cliMsgIdUndo` (client id của tin bị thu hồi). Sau đó Zalo đẩy một tin `chat.undo` như khi thu hồi từ điện thoại; gateway đã áp dụng thu hồi nên bỏ qua nó.

## 8e. Thành viên nhóm và hồ sơ người dùng (đã kiểm chứng)

`getmg-v2` (thông tin nhóm) có `memVerList`: danh sách `"<uid>_<phiên bản>"` của mọi thành viên, kèm `creatorId` và `adminIds`. Thử trên 4 nhóm 10 đến 28 người: Zalo liệt kê đủ id của tất cả thành viên.

Tên và ảnh của người không phải bạn bè: `POST {profile}/api/social/group/members` với `friend_pversion_map: ["<uid>_0", …]`, đúng như Zalo PC làm với thành viên nhóm nó chưa biết. Kết quả nằm trong `profiles`, là object theo uid: `{ displayName, zaloName, avatar, … }`. Đã thử hỏi 45 người một lần: Zalo trả đủ 45 hồ sơ, người nào cũng có tên và ảnh. Theo ctdozlite, `getminiprofiles` trả mã 114 nếu đưa uid trần. Gateway hỏi mỗi người tối đa 3 ngày một lần. Khi mở thông tin nhóm, gateway gọi `getmg-v2` cho riêng nhóm đó rồi hỏi tên của những thành viên chưa biết. Đã thử trên cả 4 nhóm: tất cả thành viên đều có tên, ảnh và đúng vai trò (`creatorId` là trưởng nhóm, `adminIds` là phó nhóm).

## 8f. Nhãn dán (đã kiểm chứng)

Theo ctdozlite (chép từ Zalo PC), host là `sticker` trong bảng dịch vụ (`tt-sticker-wpa.zaloapp.com`):

- Danh sách gói: `GET /api/message/sticker/category/list/v2` với `params` là chuỗi rỗng đã mã hóa (không phải JSON). Kết quả có `dataUrl`, là tệp JSON trên CDN của Zalo (có khi nén gzip) liệt kê cả kho: `{ id, name, totalImage, iconUrl }`. `exCateIds` là các gói cần bỏ. Đã thử: 353 gói.
- Nhãn dán của một gói: `GET /api/message/sticker/category/sticker_detail` với `{ cid }`, ra danh sách `{ id, cateId, type }`. Đã thử: một gói 30 nhãn.
- Tìm theo từ khóa: `/api/message/sticker/suggest/stickers` (`{ keyword, gif: 1, guggy: 0, imei }`, kết quả trong `sugg_sticker`) và `/api/message/sticker/search` (`{ keyword, limit, srcType: 0, imei }`, ra danh sách); gateway gộp hai kết quả. Đã thử: "haha" ra 93 nhãn, "cảm ơn" ra 26 nhãn.
- Gửi: `POST {chat}/api/message/sticker` (nhóm: `{group}/api/group/sticker`) với `toid` hoặc `grid`, `stickerId`, `cateId`, `type`, `clientId`, `imei`, `zsource: 101`. Đã thử gửi vào Cloud: Zalo nhận, bản sao gửi về không bị lưu trùng.
- Ảnh: `https://api.zaloapp.com/api/emoticon/sticker/webpc?eid=<id>&size=130`; 30 nhãn dán có sẵn (gói 0) nằm ở `stc-chat.zdn.vn`. Ảnh của gói và của nhãn dán tải được không cần cookie.

## 8g. Tìm người và kết bạn (một phần đã kiểm chứng)

Đều trên dịch vụ `friend`, theo zca-js và ctdozlite:

- Tìm theo số điện thoại: `GET /api/friend/profile/get` với `{ phone, avatar_size, language: "vi", imei, reqSrc: 40 }`; số Việt Nam bắt đầu bằng 0 được đổi thành 84. Kết quả có `uid`, `display_name`, `zalo_name`, `avatar`. Mã `216`: không có tài khoản, hoặc chủ số không cho tìm bằng số điện thoại. Đã thử với một số không hợp lệ: Zalo trả mã `219` "Số điện thoại không hợp lệ", tức là yêu cầu tới đúng chỗ. Chưa thử với số thật.
- Quan hệ: `GET /api/friend/reqstatus` với `{ fid, imei }` cho `is_friend`, `is_requested` (mình đã mời) và `is_requesting` (họ đã mời). Đã thử với một người bạn: đúng là bạn bè.
- Lời mời đang chờ: `GET /api/friend/recommendsv2/list` với `{ imei }`. Lời mời là các mục có `dataInfo.recommType` bằng 2 (1 là gợi ý kết bạn), lời nhắn ở `dataInfo.recommInfo.message`. Đã gọi được, nhưng tài khoản thử chưa có lời mời nào, nên chưa thấy dữ liệu thật.
- Gửi lời mời `POST /api/friend/sendreq` (`{ toid, msg, reqsrc: 30, imei, language }`), đồng ý `POST /api/friend/accept` (`{ fid, language }`), từ chối `POST /api/friend/reject` (`{ fid, imei }`): chưa thử, vì sẽ tác động tới người thật.

## 8h. Chuyển tiếp và xóa ở phía mình (đã kiểm chứng)

Chuyển tiếp: `POST {file}/api/message/mforward` (nhóm: `{file}/api/group/mforward`) như zca-js `forwardMessage`, với `toIds: [{ clientId, toUid, ttl }]` và `imei` (nhóm: `grids: [{ clientId, grid, ttl }]`, không có `imei`), `ttl`, `msgType` (chuỗi), `totalIds`, `msgInfo` (chuỗi JSON `{ message }`) và `decorLog: "null"`. Kết quả gồm `success: [{ clientId, msgId }]` và `fail: [{ clientId, error_code }]`. Đã thử vào Cloud: tin chữ đi được. Với ảnh (`msgType` 32) và tệp (46), đưa nội dung gốc của tin vào `msgInfo` thì Zalo trả mã 114 "Tham số không hợp lệ". Vì vậy gateway gửi lại ảnh, video, tin nhắn thoại và tệp từ bản nó giữ (đã thử với một ảnh 6 MB) và gửi lại nhãn dán theo id.

Xóa ở phía mình: `POST {chat}/api/message/delete` (nhóm: `{group}/api/group/deletemsg`) với `toid` hoặc `grid`, `cliMsgId` mới, `msgs: [{ cliMsgId, globalMsgId, ownerId, destId }]`, `onlyMe: 1`; chat 1-1 thêm `imei`. Đã thử trong Cloud: Zalo nhận, tin không quay lại.

## 8i. Lần truy cập cuối (gọi được, chưa thấy thời gian thật)

`GET {profile}/api/social/profile/lastOnline` với `{ uid, conv_type: 1, imei }`, như zca-js, trả về `{ lastOnline, settings: { show_online_status } }`. `lastOnline` có thể là giây hoặc mili giây (ctdozlite xử lý cả hai). Đã gọi cho cả 5 người bạn của tài khoản thử: đúng cấu trúc trên, 2 người cho hiện trạng thái, nhưng `lastOnline` của mọi người đều bằng 0. Có lẽ vì chính tài khoản thử đang tắt "Hiện trạng thái truy cập": Zalo thường không cho người đã tắt xem trạng thái của người khác.

## 8j. Đọc trên thiết bị khác, tin đã tới và đã xem

Khi tài khoản đọc một hội thoại trên thiết bị khác (điện thoại), Zalo đẩy khung **504** có `clearUnreads: [{ idTo, isGroup, lastMsgId, ts, type }]` (đôi khi thêm `lastCliMsgId`, `sct`). Đã thấy trong log thật nhiều lần, đúng lúc có hội thoại được đọc ở nơi khác. Gateway hạ số tin chưa đọc xuống còn số tin đến sau `lastMsgId`, và báo `thread_read` cho các client. zca-js không xử lý khung này.

Tin của chính mình: khung **502** (chat 1-1) có `delivereds: [{ msgId, realMsgId, seen, deliveredUids, seenUids, mSTs }]` và `seens: [{ idTo, msgId, realMsgId }]`; khung **522** (nhóm) có `delivereds` kèm `groupId` và `groupSeens: [{ msgId, groupId, seenUids }]`, như zca-js đọc. Khung 502 có `delivereds` đã thấy thật, mỗi lần khoảng 2 giây sau khi gửi vào Cloud lúc điện thoại đang mở. Gateway tìm tin theo id (mã Zalo, hoặc client id), đưa nó và các tin trước nó lên "đã nhận" hay "đã xem". Việc khớp id với dữ liệu thật chưa kiểm chứng: lần thử sau cùng, Zalo không gửi khung 502 nào trong 30 giây.

## 8k. Nhắc tên trong nhóm (theo zca-js, chưa kiểm chứng)

Gửi: tin nhóm có nhắc tên đi tới `{group}/api/group/mention` thay cho `/api/group/sendmsg`, thêm `mentionInfo`: chuỗi JSON `[{ pos, uid, len, type }]`, `type` 1 khi `uid` là "-1" (cả nhóm). Vị trí tính theo đơn vị UTF-16, vì client của Zalo đếm như chuỗi JavaScript. Tin trả lời vẫn đi `/quote` và mang thêm `mentionInfo`. Chưa thử, vì phải gửi vào một nhóm có người thật.

Nhận: tin có trường `mentions: [{ uid, pos, len, type }]` (kiểu `TMessage` của zca-js). Gateway lưu chúng (schema v7) và client tô tên được nhắc. Trong các tin thật đã lưu chưa có tin nào nhắc tên.

## 8l. Tạo nhóm (theo zca-js, chưa kiểm chứng)

`POST {group}/api/group/create/v2`, tham số đã mã hóa nằm trên URL (`params`), thân để trống, như zca-js `createGroup`: `clientId`, `gname` (tên, hoặc thời điểm tạo khi để Zalo tự đặt), `gdesc: null`, `members`, `membersTypes` (-1 cho mỗi người), `nameChanged` (1 khi có tên), `createLink: 1`, `clientLang`, `imei`, `zsource: 601`. Kết quả gồm `groupId`, `sucessMembers` (Zalo viết sai chính tả như vậy) và `errorMembers`. Gateway hỏi lại `getmg-v2` để lấy tên và ảnh Zalo đặt. Chưa thử, vì tạo nhóm sẽ thêm người thật vào.

## 9. Báo đã xem (theo zca-js, chưa kiểm chứng)

`POST {chat}/api/message/seenv2?…&nretry=0` (nhóm: `{group}/api/group/seenv2`) với `params` mã hóa: `msgInfos` là chuỗi JSON `{ "data": [ … ], "grid": <id nhóm, chỉ với nhóm> }`, kèm `senderId` (người gửi, với hội thoại 1-1) hoặc `imei` (với nhóm). Mỗi phần tử của `data` lấy từ tin nhắn gốc: `cmi` (`cliMsgId`), `gmi` (`msgId`), `si` (`uidFrom`), `di` (`idTo`, hoặc `"0"` khi là chính mình), `mt` (`msgType`), `st`, `at`, `cmd`, `ts`. Tối đa 50 tin mỗi lần.

## 10. Lịch sử cũ (chưa tìm ra)

Hội thoại 1-1 không có lịch sử trên máy chủ: Zalo PC lấy lịch sử bằng cách đồng bộ từ điện thoại. Với nhóm và Cloud, ctdozlite gọi `GET {group_cloud_message}/api/cm/getoldv2` và `cm/getrecentv2` với `groupId`, `globalMsgId`, `count`, `msgIds: []`, `imei`, `src`. Thử trên tài khoản thử (tháng 11/2026), các lời gọi này trả lỗi `604` (hoặc `-69` "Stop retry" khi `src: 3`), còn `{group}/api/group/history` trả `404`. Hiện gateway chỉ có những tin đến sau khi nó chạy, cộng với tin gần đây của `get-last-msgs`. Lệnh `zeta probe` (ví dụ `zeta`) giúp thử tiếp mà không in nội dung tin.

## 11. Cloud của tôi

"Cloud của tôi" là một hội thoại riêng có mã `send2me_id` (trả về ở `getLoginInfo`), không phải `uid` của chính tài khoản. Gửi ảnh với `toid` là `uid` của mình bị từ chối với lỗi `114` (tham số không hợp lệ).

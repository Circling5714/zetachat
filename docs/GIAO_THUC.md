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
- Ảnh: `https://api.zaloapp.com/api/emoticon/sticker/webpc?eid=<id>&size=130`; 30 nhãn dán có sẵn (gói 0) nằm ở `stc-chat.zdn.vn`. Ảnh của gói và của nhãn dán tải được không cần cookie. Đây chỉ là khung hình đầu.
- Nhãn động: `GET /api/message/sticker/sticker_detail` với `{ sid }` trả về `stickerSpriteUrl` (`https://zalo-api.zadn.vn/api/emoticon/sprite?eid=<id>&size=130`), `totalFrames` và `duration` (thời gian **mỗi khung**, mili giây); `stickerWebpUrl` thường là `null`. Đã kiểm chứng: sprite là một dải PNG cao 130, rộng `totalFrames × 130` (nhãn 101001: 10 khung × 70 ms; nhãn 1111: 2 khung × 300 ms). Sprite của nhãn tĩnh chỉ có một khung. Client chạy dải đó theo từng bước bằng CSS, như Zalo PC.
- `totalFrames` bằng **0** nghĩa là "xem sprite". Với nhãn tĩnh, sprite là đúng một hình 130×130. Với nhãn mới hơn (`ext` 2, đã gặp nhãn 40137), URL sprite trả về **ảnh WebP động** (370×300), `duration` khi đó là cả vòng (3000 ms). Vì vậy client đo sprite rồi mới quyết: dải ngang thì chạy từng khung, hình khác thì hiện thẳng (trình duyệt tự phát WebP động), hình giống ảnh tĩnh thì giữ nguyên.

## 8g. Tìm người và kết bạn (một phần đã kiểm chứng)

Đều trên dịch vụ `friend`, theo zca-js và ctdozlite:

- Tìm theo số điện thoại: `GET /api/friend/profile/get` với `{ phone, avatar_size, language: "vi", imei, reqSrc: 40 }`; số Việt Nam bắt đầu bằng 0 được đổi thành 84. Kết quả có `uid`, `display_name`, `zalo_name`, `avatar`. Mã `216`: không có tài khoản, hoặc chủ số không cho tìm bằng số điện thoại. Đã thử với một số không hợp lệ: Zalo trả mã `219` "Số điện thoại không hợp lệ", tức là yêu cầu tới đúng chỗ. Chưa thử với số thật.
- Quan hệ: `GET /api/friend/reqstatus` với `{ fid, imei }` cho `is_friend`, `is_requested` (mình đã mời) và `is_requesting` (họ đã mời). Đã thử với một người bạn: đúng là bạn bè.
- Lời mời đang chờ: `GET /api/friend/recommendsv2/list` với `{ imei }`. Lời mời là các mục có `dataInfo.recommType` bằng 2 (1 là gợi ý kết bạn), lời nhắn ở `dataInfo.recommInfo.message`. Đã gọi được, nhưng tài khoản thử chưa có lời mời nào, nên chưa thấy dữ liệu thật.
- Gửi lời mời `POST /api/friend/sendreq` (`{ toid, msg, reqsrc: 30, imei, language }`), đồng ý `POST /api/friend/accept` (`{ fid, language }`), từ chối `POST /api/friend/reject` (`{ fid, imei }`): chưa thử, vì sẽ tác động tới người thật.

## 8h. Chuyển tiếp và xóa ở phía mình (đã kiểm chứng)

Chuyển tiếp: `POST {file}/api/message/mforward` (nhóm: `{file}/api/group/mforward`) như zca-js `forwardMessage`, với `toIds: [{ clientId, toUid, ttl }]` và `imei` (nhóm: `grids: [{ clientId, grid, ttl }]`, không có `imei`), `ttl`, `msgType` (chuỗi), `totalIds`, `msgInfo` (chuỗi JSON `{ message }`) và `decorLog: "null"`. Kết quả gồm `success: [{ clientId, msgId }]` và `fail: [{ clientId, error_code }]`. Đã thử vào Cloud: tin chữ đi được. Với ảnh (`msgType` 32) và tệp (46), đưa nội dung gốc của tin vào `msgInfo` thì Zalo trả mã 114 "Tham số không hợp lệ". Vì vậy gateway gửi lại ảnh, video, tin nhắn thoại và tệp từ bản nó giữ (đã thử với một ảnh 6 MB) và gửi lại nhãn dán theo id.

Xóa ở phía mình trên thiết bị khác (điện thoại): Zalo đẩy về một tin `msgType` `chat.delete`, `content` là danh sách `[{ globalDelMsgId, clientDelMsgId, destId, uidFrom, uidTo, type, actionType }]` (đã thấy thật). Gateway xóa các tin đó và không lưu `chat.delete` như một tin (bản cũ có lưu, nên hiện thành dòng "[chat.delete]"; schema v9 dọn chúng).

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

## 8m. Tên gợi nhớ, tên của người lạ, đổi tên nhóm

Tên gợi nhớ nằm ở dịch vụ `alias` (`tt-alias-wpa.api.zaloapp.com` trong bảng dịch vụ), theo zca-js: `GET /api/alias/list` với `{ page, count, imei }` trả về `{ items: [{ userId, alias }], updateTime }`; `GET /api/alias/update` với `{ friendId, alias, imei }`; `GET /api/alias/remove` với `{ friendId }`. Đã kiểm chứng: danh sách đọc được 2 tên đặt từ điện thoại; đặt rồi bỏ một tên đều thành công, số tên quay về như cũ. Gateway đọc lại danh sách mỗi khi phiên khởi động.

Chat 1-1 với người chưa kết bạn không có tên trong danh sách bạn bè. Gateway lấy tên và ảnh của họ qua `social/group/members` (mục 8e), và đặt tên "Cloud của tôi" cho hội thoại `send2me_id`. Đã kiểm chứng: cả 7 chat 1-1 của tài khoản thử đều có tên.

Đổi tên nhóm: `POST {group}/api/group/updateinfo` với `{ grid, gname, imei }` (zca-js `changeGroupName`). Chưa thử, vì mọi thành viên sẽ thấy tên mới.

## 8n. Chữ có định dạng, tin quan trọng và khẩn cấp (đã kiểm chứng)

Gửi: thêm vào tham số của `message/sms`, `group/sendmsg`, `group/mention` hay `/quote` hai trường, như Zalo Web và zca-js. `textProperties` là chuỗi JSON `{ "styles": [{ "start", "len", "st" }], "ver": 0 }`, vị trí theo UTF-16; mã `st` xem mục `/threads/{id}/messages` trong `docs/API.md`, trong đó thụt lề là `ind_<n>0`. `metaData: { "urgency": 1 | 2 }` đánh dấu tin quan trọng hay khẩn cấp.

Nhận (đã thấy thật, gửi vào Cloud): tin có định dạng vẫn là `webchat`, nhưng `content` là một object `{ action: "rtf", title: <chữ>, params: <chuỗi JSON { styles, ver }> }`. Mức quan trọng nằm ở trường `specialMsgType` của tin: 1 là quan trọng, 2 là khẩn cấp. Gateway lưu hai thứ này (schema v9, cột `styles`, `urgency`). Khi trả lời một tin có định dạng, gateway trích nó như chữ thường.

## 8o. Danh thiếp (đã kiểm chứng)

Gửi, như zca-js `sendCard`: hỏi `POST {friend}/api/friend/mget-qr` với `{ fids: [uid] }`, được `{ "<uid>": "<ảnh QR trên qr-talk.zdn.vn>" }`. Rồi gửi `POST {file}/api/message/forward` (nhóm: `{file}/api/group/forward`) với `msgType` 6, `clientId`, `ttl: 0` và `msgInfo` là chuỗi JSON `{ contactUid, qrCodeUrl, phone? }`. Chat 1-1 thêm `toId` (chữ I hoa) và `imei`, nhóm thêm `grid` và `visibility: 0`. Kết quả có `msgId`.

Nhận (đã thấy thật, gửi danh thiếp của chính tài khoản vào Cloud): `msgType` `chat.recommended`. `content` là `{ action: "recommened.user" (Zalo viết sai như vậy), title: <tên>, thumb: <ảnh đại diện>, params: <uid>, href: "www.zaloapp.com", description: <chuỗi JSON { qrCodeUrl, gUid, phone? }> }`. Liên kết chia sẻ cũng là `chat.recommended`, nhưng `action` khác. Bản cũ hiện danh thiếp thành "[Liên kết]"; schema v10 đọc lại chúng.

## 8p. Tài khoản ngân hàng (đã kiểm chứng)

Dịch vụ `zimsg` (`zimsg.chat.zalo.me`), như zca-js:

- Danh sách ngân hàng: `GET /api/transfer/conf` với `{}` trả về `{ banks: [{ bin, logo, name, name_eng, short_name, search_key_word }] }`. Thử thật: 66 ngân hàng, đều có logo; `search_key_word` là các tên gọi khác, nối bằng `|` (ví dụ `abbank|an binh`).
- Tài khoản đã lưu: `GET /api/transfer/list` với `{ page, limit }` trả về `{ hasMore, total, myBanks: [{ id, account_id, bin, bank_number, holder_name, is_default, … }] }`. Lưu: `POST /api/transfer/create` với `{ bin, bank_number, holder_name, language }`. Xóa: `POST /api/transfer/delete` với `{ account_id, is_default, language }`. Đã thử thật với một số giả (mười số 0): lưu được, danh sách hiện nó là mặc định, xóa xong danh sách trống lại. Tên chủ tài khoản phải viết hoa không dấu, ít nhất 5 chữ (như `normalizeHolderName` của zca-js). Mỗi lần thêm hay xóa, Zalo đẩy một khung điều khiển 601 mang tài khoản đó.
- Gửi thẻ: `POST /api/transfer/card` với `{ binBank, numAccBank, nameAccBank, cliMsgId, tsMsg, destUid, destType }` (`destType` 1 cho nhóm). Zalo trả lời rỗng và tự tạo tin, với `cliMsgId` riêng của nó (thấy thật: lệch 18 ms so với mã zeta gửi). Vì vậy gateway khớp tin vọng về theo thời gian, không theo mã.

Nhận (đã thấy thật): `msgType` `chat.webcontent`, `content.action` `zinstant.bankcard`. `params` chứa `customMsg.msg` là `{ en: "[Bank account]", vi: "[Tài khoản ngân hàng]" }`, cùng hai mẫu hiển thị trên `zinst-stc.zadn.vn`: `item` (nhị phân, cho điện thoại) và `pcItem` (HTML, cho Zalo PC). Mẫu HTML có:

- `<p id="bank_bin">970436</p>`;
- tên ngân hàng, số tài khoản (`number_text`), chủ tài khoản (`pc_element`);
- các liên kết `https://bankcard-action.zalo.me?action=transfer|save&content=<chuỗi VietQR>`.

Gateway đọc mẫu này khi có client xem thẻ lần đầu. Mã ngân hàng và số tài khoản lấy từ chuỗi VietQR (có kiểm tra CRC), còn chủ tài khoản lấy từ đoạn văn bản.

VietQR là chuẩn EMVCo: `00 02 01`, `01 02 11|12`, `38` (`00` `A000000727`, `01` (`00` BIN, `01` số tài khoản), `02` `QRIBFTTA`), `53 03 704`, `54` số tiền, `58 02 VN`, `62` (`08` nội dung), `63 04` CRC-16/CCITT-FALSE. Thẻ của Zalo thêm trường `80` (`com.vng.zalo`, `ZABANKCARD`). Gateway tự tạo mã QR (`/vietqr.svg`) cho thẻ và cho số tài khoản nhận ra trong tin nhắn.

## 8q. Tin nhắn nhanh (đã kiểm chứng)

Dịch vụ `quick_message` (`quickmessage-wpa.zaloapp.com`), mọi lời gọi đều là GET với tham số mã hóa, như zca-js và Zalo Web:

- Danh sách: `/api/quickmessage/list` với `{ version: 0, lang: 0, imei }` trả về `{ cursor, version, items: [{ id, keyword, type, message: { title, params }, media, createdTime, lastModified }] }`.
- Thêm: `/api/quickmessage/create` với `{ keyword, message: { title, params: "" }, type: 0, imei }`.
- Sửa: `/api/quickmessage/update` với `{ itemId, keyword, message, type: 0 }`.
- Xóa: `/api/quickmessage/delete` với `{ itemIds }`.

Tin `type` 1 có thêm ảnh trong `media.items`.

Đã thử thật: tài khoản thử có 1 tin. Thêm tin thứ hai thì Zalo trả mã **821** "Item size not support". `getServerInfo` cho biết lý do: `quick_message.max_creation_quantity` bằng 1, tức tài khoản cá nhân chỉ được giữ 1 tin; Zalo Web hiện "Tin nhắn nhanh (n/tối đa)". Mã 820 là phím tắt trùng. Vì vậy zetachat tự giữ các tin còn lại (schema v12, bảng `quick_messages`) và gộp với tin của Zalo.

## 8r. Bình chọn, ghi chú, nhắc hẹn

Tham số lấy theo zca-js; xem `crates/zeta-zalo/src/board.rs`. Bình chọn đi qua dịch vụ `group`, ghi chú và nhắc hẹn qua `group_board` (`groupboard-wpa.api.zaloapp.com`).

Đã kiểm chứng (chỉ đọc, trên các nhóm thật):

- `GET /api/board/list` trả về `{ count, items: [{ boardType, data }] }`. `boardType` 3 là bình chọn: `data` có `poll_id`, `question`, `options: [{ option_id, content, votes, voted, voters }]`, `closed`, `num_vote`… (đọc được 2 bình chọn). `boardType` 2 là tin ghim: `params` là chuỗi JSON `{ client_msg_id, global_msg_id, msg_type, senderName, senderUid, thumb, title }`.
- `POST /api/poll/detail` trả về bình chọn như trên.
- `GET /api/board/listReminder` trả `data` là chuỗi JSON của danh sách nhắc hẹn (`id`, `params: { title }`, `startTime`, `repeat`, `responseMem: { acceptMember, rejectMember, myResp }`). Nhóm không có nhắc hẹn thì Zalo trả **mã lỗi -1** thay cho danh sách rỗng; `/api/board/oneone/list` của chat 1-1 cũng vậy.

Đã kiểm chứng (ghi, trong Cloud): tạo một nhắc hẹn 1-1 bằng `/api/board/oneone/create`, rồi xóa bằng `/api/board/oneone/remove`. Mỗi lần, Zalo đẩy vào cuộc trò chuyện:

- một tin `webchat` có `content.action` là `msginfo.actionlist`. `title` là câu đã ghép sẵn ("… tạo nhắc hẹn mới <tiêu đề> - 17:53.", "… xóa nhắc hẹn …"); `params` có mẫu câu theo ngôn ngữ, `iconUrl` và nút (`actionLabelv2`: "Xem", "Tạo mới"). Người gửi là tài khoản Cloud, tên cũ "Truyền File".
- khi tạo: một tin `chat.ecard` (thiệp): `title` "⏰ <tiêu đề>", `description` là giờ hẹn ("Thứ Năm, 5 tháng 11 lúc 17:53"), `thumb` là ảnh, `params.previewTxt`, và trong `params.actions` có `actionId` `action.open.reminder` kèm dữ liệu nhắc hẹn (`create_reminder11`).

Client vẽ `msginfo.actionlist` thành dòng giữa khung chat, như Zalo. Schema v13 đọc lại các tin đã lưu.

Tin bình chọn trong nhóm là `group.poll`. Theo mã Zalo Web, `content.params` là chuỗi JSON có `pollId` và `question`; chưa thấy thật.

Chưa thử, vì phải ghi vào nhóm có người thật: tạo bình chọn, bình chọn, thêm phương án, khóa bình chọn, tạo ghi chú và nhắc hẹn nhóm.

## 8s. Trả lời tự động

Zalo có dịch vụ `auto_reply` riêng (`auto-rep-msg.chat.zalo.me`, zca-js `createAutoReply`…). `getServerInfo` cho tài khoản thử `autoReplies: { max_rules: 5, max_active_rules: 5 }`. Một quy tắc của Zalo chỉ có một câu trả lời, một khoảng thời gian và phạm vi (mọi người, người lạ, vài người). zetachat không dùng dịch vụ này. Gateway tự chạy kịch bản: có từ khóa, lịch theo ngày trong tuần, điều kiện "khi vắng mặt", thời gian chờ, nhắc tên trong nhóm, nhật ký và nút thử. Tin trả lời được gửi như mọi tin chữ khác.

## 9. Báo đã xem (theo zca-js, chưa kiểm chứng)

`POST {chat}/api/message/seenv2?…&nretry=0` (nhóm: `{group}/api/group/seenv2`) với `params` mã hóa: `msgInfos` là chuỗi JSON `{ "data": [ … ], "grid": <id nhóm, chỉ với nhóm> }`, kèm `senderId` (người gửi, với hội thoại 1-1) hoặc `imei` (với nhóm). Mỗi phần tử của `data` lấy từ tin nhắn gốc: `cmi` (`cliMsgId`), `gmi` (`msgId`), `si` (`uidFrom`), `di` (`idTo`, hoặc `"0"` khi là chính mình), `mt` (`msgType`), `st`, `at`, `cmd`, `ts`. Tối đa 50 tin mỗi lần.

## 10. Lịch sử cũ (chưa tìm ra)

Hội thoại 1-1 không có lịch sử trên máy chủ: Zalo PC lấy lịch sử bằng cách đồng bộ từ điện thoại. Với nhóm và Cloud, ctdozlite gọi `GET {group_cloud_message}/api/cm/getoldv2` và `cm/getrecentv2` với `groupId`, `globalMsgId`, `count`, `msgIds: []`, `imei`, `src`. Thử trên tài khoản thử (tháng 11/2026), các lời gọi này trả lỗi `604` (hoặc `-69` "Stop retry" khi `src: 3`), còn `{group}/api/group/history` trả `404`. Hiện gateway chỉ có những tin đến sau khi nó chạy, cộng với tin gần đây của `get-last-msgs`. Lệnh `zeta probe` (ví dụ `zeta`) giúp thử tiếp mà không in nội dung tin.

## 11. Cloud của tôi

"Cloud của tôi" là một hội thoại riêng có mã `send2me_id` (trả về ở `getLoginInfo`), không phải `uid` của chính tài khoản. Gửi ảnh với `toid` là `uid` của mình bị từ chối với lỗi `114` (tham số không hợp lệ).

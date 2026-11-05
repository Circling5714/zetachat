//! Developer CLI for `zeta-zalo`. It exists to exercise the protocol crate against a
//! real (test) account; it is not the zetachat product.
//!
//! ```text
//! cargo run --example zeta -- login   <session.json> [computer name]
//! cargo run --example zeta -- check   <session.json>
//! cargo run --example zeta -- friends <session.json> [--names]
//! cargo run --example zeta -- send    <session.json> user|group <id> <text>
//! cargo run --example zeta -- listen  <session.json> [seconds]
//! cargo run --example zeta -- import-ctdozlite <ctdozlite session.json> <out.json>
//! ```
//!
//! Output never contains credentials. Set `RUST_LOG=debug` for protocol logs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use zeta_zalo::api::{Recipient, SessionHealth, TextExtras};
use zeta_zalo::board::{BoardItem, NewReminder};
use zeta_zalo::login::QrEvent;
use zeta_zalo::socket::{self, RealtimeEvent};
use zeta_zalo::{Device, SessionData, ZaloClient};

const USAGE: &str = "\
Cách dùng:
  zeta login   <session.json> [tên máy]          đăng nhập bằng mã QR, lưu phiên
  zeta check   <session.json>                    hỏi Zalo phiên còn dùng được không
  zeta friends <session.json> [--names]          đếm (hoặc liệt kê) bạn bè
  zeta groups  <session.json> [--names]          đếm (hoặc liệt kê) nhóm
  zeta people  <session.json>                    hỏi tên, ảnh của thành viên nhóm không phải bạn bè (chỉ in số đếm)
  zeta stickers <session.json> [packs | pack <id> | search <từ khóa>]   kho nhãn dán của Zalo
  zeta friend  <session.json> [requests | status [uid] | find <số>]     lời mời kết bạn, quan hệ, tìm theo số (chỉ đọc)
  zeta aliases <session.json>                    đếm tên gợi nhớ tài khoản đã đặt
  zeta forward <session.json> user|group <id> <msgType> <msgInfo JSON | @tệp>   chuyển tiếp qua mforward
  zeta recent  <session.json>                    cấu trúc tin nhắn gần đây (không in nội dung)
  zeta probe   <session.json> <dịch vụ> <đường dẫn> <JSON params>  gọi GET bất kỳ, in cấu trúc phản hồi
  zeta limits  <session.json>                    giới hạn gửi tệp do máy chủ cấp (getServerInfo)
  zeta send    <session.json> user|group <id> <nội dung> [--styles JSON] [--urgency 0|1|2]
  zeta card    <session.json> user|group <id> <uid | me> [số điện thoại]   gửi danh thiếp
  zeta bank    <session.json> banks | accounts | send user|group <id> <bin> <số TK> <tên>
  zeta quick   <session.json> list | add <từ khóa> <nội dung> | remove <id>   tin nhắn nhanh
  zeta board   <session.json> list <group id> | reminders user|group <id> | remind user|group <id> <tiêu đề> <phút nữa> | unremind user|group <id> <reminder id> | poll <poll id>
      (id \"cloud\" với user là \"Cloud của tôi\")
  zeta react   <session.json> user|group <id> <msgId> <cliMsgId> <biểu tượng, \"\" để gỡ>
  zeta listen  <session.json> [giây]             nghe sự kiện thời gian thực
  zeta import-ctdozlite <ctdozlite.json> <out.json>";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("login") => login(rest).await,
        Some("check") => check(rest).await,
        Some("friends") => friends(rest).await,
        Some("groups") => groups(rest).await,
        Some("people") => people(rest).await,
        Some("stickers") => stickers(rest).await,
        Some("friend") => friend(rest).await,
        Some("forward") => forward(rest).await,
        Some("presence") => presence(rest).await,
        Some("aliases") => aliases(rest).await,
        Some("recent") => recent(rest).await,
        Some("probe") => probe(rest).await,
        Some("limits") => limits(rest).await,
        Some("send") => send(rest).await,
        Some("card") => card(rest).await,
        Some("bank") => bank(rest).await,
        Some("quick") => quick(rest).await,
        Some("board") => board(rest).await,
        Some("react") => react(rest).await,
        Some("listen") => listen(rest).await,
        Some("import-ctdozlite") => import_ctdozlite(rest),
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

fn session_path(args: &[String]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        args.first().context("thiếu đường dẫn tới file phiên")?,
    ))
}

fn load_client(path: &Path) -> Result<ZaloClient> {
    let session = SessionData::load(path)
        .with_context(|| format!("không đọc được phiên {}", path.display()))?;
    Ok(ZaloClient::from_session(session)?)
}

/// `user <id>` or `group <id>`; `user cloud` is the account's "Cloud của tôi",
/// so tests need not name it.
fn recipient<'a>(client: &'a ZaloClient, kind: &str, id: &'a str) -> Result<Recipient<'a>> {
    match (kind, id) {
        ("user", "cloud") => client
            .session()
            .map(|s| s.send2me_id.as_str())
            .filter(|s| !s.is_empty())
            .map(Recipient::User)
            .context("phiên không có send2me_id"),
        ("user", id) => Ok(Recipient::User(id)),
        ("group", id) => Ok(Recipient::Group(id)),
        (other, _) => bail!("loại người nhận không hợp lệ: {other}"),
    }
}

fn print_qr(payload: &str) {
    match qrcode::QrCode::new(payload.as_bytes()) {
        Ok(code) => println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build()
        ),
        Err(e) => println!("(không vẽ được mã QR: {e})"),
    }
}

async fn login(args: &[String]) -> Result<()> {
    let path = session_path(args)?;
    let name = args.get(1).cloned().unwrap_or_else(|| "zetachat".into());
    // Reuse the device identity of an earlier session so Zalo sees the same machine.
    let device = SessionData::load(&path)
        .map(|s| s.device)
        .unwrap_or_else(|_| Device::generate(name));
    let mut client = ZaloClient::new(device)?;
    let session = client
        .login_qr("vi", |event| match event {
            QrEvent::Ticket(t) => {
                print_qr(&t.payload);
                println!(
                    "QR_READY: quét bằng app Zalo trong {} giây",
                    t.timeout.as_secs()
                );
            }
            QrEvent::Scanned(_) => println!("Đã quét, hãy bấm \"Đăng nhập\" trên điện thoại…"),
            QrEvent::Confirmed => println!("Đã xác nhận, đang tải khóa phiên…"),
        })
        .await?;
    session.save(&path)?;
    println!("LOGIN_OK: đã lưu phiên vào {}", path.display());
    Ok(())
}

async fn check(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    match client.check_session().await {
        SessionHealth::Valid(_) => println!("VALID"),
        SessionHealth::Rejected => {
            println!("EXPIRED");
            std::process::exit(1);
        }
        SessionHealth::Unknown(reason) => {
            println!("UNREACHABLE: {reason}");
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn friends(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let list = client.friends().await?;
    println!("{} bạn bè", list.len());
    if args.iter().any(|a| a == "--names") {
        for f in &list {
            println!("  {} {}", f.uid, f.display_name);
        }
    }
    Ok(())
}

async fn groups(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let list = client.groups().await?;
    let named = list.iter().filter(|g| !g.name.is_empty()).count();
    println!("{} nhóm ({named} có tên)", list.len());
    for g in &list {
        println!(
            "  nhóm: {} thành viên, Zalo liệt kê {} id, {} quản trị",
            g.member_count,
            g.member_ids.len(),
            g.admin_ids.len()
        );
    }
    if args.iter().any(|a| a == "--names") {
        for g in &list {
            println!("  {} {} ({} thành viên)", g.id, g.name, g.member_count);
        }
    }
    Ok(())
}

/// How many contacts have a name given by the account ("tên gợi nhớ").
async fn aliases(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let list = client.aliases().await?;
    let friends: Vec<String> = client.friends().await?.into_iter().map(|f| f.uid).collect();
    let of_friends = list.iter().filter(|(uid, _)| friends.contains(uid)).count();
    println!("{} tên gợi nhớ ({of_friends} cho bạn bè)", list.len());
    Ok(())
}

/// When each friend was last on Zalo, described coarsely (hidden or not, how
/// old), never the time itself.
async fn presence(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    for f in client.friends().await? {
        let seen = client.last_online(&f.uid).await?;
        let age = match seen.at {
            0 => "không có".to_string(),
            at if now - at < 3_600_000 => "trong giờ qua".into(),
            at if now - at < 86_400_000 => "trong ngày".into(),
            _ => "lâu hơn một ngày".into(),
        };
        println!("hiện trạng thái: {}, lần cuối: {age}", seen.visible);
    }
    Ok(())
}

/// Forward through `mforward`: `msgInfo` is JSON given inline or read from a
/// file (`@path`). Prints whether Zalo took it, not the content.
async fn forward(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let (kind, id) = (
        args.get(1).context("thiếu user|group")?,
        args.get(2).context("thiếu id")?,
    );
    let to = match kind.as_str() {
        "user" => Recipient::User(id),
        "group" => Recipient::Group(id),
        other => bail!("không hiểu \"{other}\": user | group"),
    };
    let msg_type: i64 = args.get(3).context("thiếu msgType")?.parse()?;
    let info = args.get(4).context("thiếu msgInfo")?;
    let info = match info.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)?,
        None => info.clone(),
    };
    let info: serde_json::Value = serde_json::from_str(&info)?;
    match client.forward(to, msg_type, &info).await {
        Ok(sent) => println!(
            "Zalo nhận: msgId {}",
            if sent.msg_id.is_empty() {
                "trống"
            } else {
                "có"
            }
        ),
        Err(e) => println!("Zalo không nhận: {e}"),
    }
    Ok(())
}

/// Read-only friend calls: requests waiting, where the account stands with its
/// first friend (or a uid), whether a phone number has an account. Prints no
/// names.
async fn friend(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    match args.get(1).map(String::as_str).unwrap_or("requests") {
        "requests" => {
            let list = client.friend_requests().await?;
            let with_message = list.iter().filter(|r| !r.message.is_empty()).count();
            println!(
                "{} lời mời kết bạn đang chờ ({with_message} có lời nhắn)",
                list.len()
            );
        }
        "status" => {
            let uid = match args.get(2) {
                Some(uid) => uid.clone(),
                None => {
                    client
                        .friends()
                        .await?
                        .into_iter()
                        .next()
                        .context("chưa có bạn bè nào")?
                        .uid
                }
            };
            println!("quan hệ: {:?}", client.friendship(&uid).await?);
        }
        "find" => {
            let phone = args.get(2).context("thiếu số điện thoại")?;
            match client.find_user(phone).await? {
                Some(user) => println!(
                    "tìm thấy một tài khoản (có tên: {}, có ảnh: {})",
                    !user.display_name.is_empty(),
                    !user.avatar.is_empty()
                ),
                None => println!("không có tài khoản nào với số này"),
            }
        }
        other => bail!("không hiểu \"{other}\": requests | status [uid] | find <số điện thoại>"),
    }
    Ok(())
}

/// The sticker store: packs, the stickers of a pack, a search. Prints counts
/// and ids (stickers are public, nothing personal).
async fn stickers(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    match args.get(1).map(String::as_str).unwrap_or("packs") {
        "packs" => {
            let packs = client.sticker_packs().await?;
            let with_icon = packs.iter().filter(|p| !p.icon.is_empty()).count();
            println!("{} gói nhãn dán ({with_icon} có ảnh)", packs.len());
            for p in packs.iter().take(5) {
                println!("  {} {} ({} nhãn)", p.id, p.name, p.count);
            }
        }
        "pack" => {
            let id: u64 = args.get(2).context("thiếu id gói")?.parse()?;
            let list = client.pack_stickers(id).await?;
            println!("gói {id}: {} nhãn dán", list.len());
            for s in list.iter().take(3) {
                println!("  {s:?} {}", s.url());
            }
        }
        "search" => {
            let keyword = args.get(2..).unwrap_or_default().join(" ");
            let list = client.search_stickers(&keyword).await?;
            println!("\"{keyword}\": {} nhãn dán", list.len());
            for s in list.iter().take(3) {
                println!("  {s:?}");
            }
        }
        "detail" => {
            // Public sticker data: printed as Zalo gives it.
            let id: u64 = args.get(2).context("thiếu id nhãn dán")?.parse()?;
            let data = client
                .get_encrypted(
                    zeta_zalo::Service::Sticker,
                    "/api/message/sticker/sticker_detail",
                    &serde_json::json!({ "sid": id }),
                )
                .await?;
            println!("{data}");
        }
        "motion" => {
            let id: u64 = args.get(2).context("thiếu id nhãn dán")?.parse()?;
            match client.sticker_motion(id).await? {
                Some(m) => println!(
                    "nhãn {id}: {} khung, {} ms
  sprite {}
  webp {}",
                    m.frames,
                    m.duration_ms,
                    m.sprite,
                    if m.webp.is_empty() {
                        "(không có)"
                    } else {
                        &m.webp
                    }
                ),
                None => println!("nhãn {id}: tĩnh"),
            }
        }
        other => {
            bail!("không hiểu \"{other}\": packs | pack <id> | search <từ khóa> | motion <id>")
        }
    }
    Ok(())
}

/// Look up the group members who are not friends (`social/group/members`);
/// prints counts only.
async fn people(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let friends: Vec<String> = client.friends().await?.into_iter().map(|f| f.uid).collect();
    let mut others: Vec<String> = Vec::new();
    for g in client.groups().await? {
        for uid in g.member_ids {
            if !friends.contains(&uid) && !others.contains(&uid) {
                others.push(uid);
            }
        }
    }
    let asked: Vec<String> = others.into_iter().take(50).collect();
    let found = client.user_profiles(&asked).await?;
    let named = found.iter().filter(|p| !p.display_name.is_empty()).count();
    let pictured = found.iter().filter(|p| !p.avatar.is_empty()).count();
    println!(
        "hỏi {} người (không phải bạn bè): Zalo trả {} hồ sơ, {named} có tên, {pictured} có ảnh",
        asked.len(),
        found.len()
    );
    Ok(())
}

/// Shape of `get-last-msgs`: key names and counts only, never message content.
async fn recent(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let data = client.recent_messages().await?;
    let root = data.get("data").filter(|d| d.is_object()).unwrap_or(&data);
    let keys: Vec<&String> = root
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    println!("khóa: {keys:?}");
    for bucket in ["msgs", "groupMsgs", "pageMsgs"] {
        if let Some(items) = root.get(bucket).and_then(|v| v.as_array()) {
            let fields: Vec<&String> = items
                .first()
                .and_then(|m| m.as_object())
                .map(|m| m.keys().collect())
                .unwrap_or_default();
            println!("{bucket}: {} tin, trường: {fields:?}", items.len());
        }
    }
    Ok(())
}

/// Keys, types and array sizes of a JSON value, never the values themselves.
fn shape(v: &serde_json::Value, depth: usize) -> String {
    use serde_json::Value;
    match v {
        Value::Object(m) if depth < 3 => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, x)| format!("{k}: {}", shape(x, depth + 1)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Object(m) => format!("{{{} khóa}}", m.len()),
        Value::Array(a) => match a.first() {
            Some(first) => format!("[{} × {}]", a.len(), shape(first, depth + 1)),
            None => "[]".into(),
        },
        Value::String(_) => "chuỗi".into(),
        Value::Number(_) => "số".into(),
        Value::Bool(_) => "bool".into(),
        Value::Null => "null".into(),
    }
}

async fn probe(args: &[String]) -> Result<()> {
    use zeta_zalo::Service;
    let client = load_client(&session_path(args)?)?;
    let (Some(service), Some(path), Some(params)) = (args.get(1), args.get(2), args.get(3)) else {
        bail!("cần: probe <session.json> <dịch vụ> <đường dẫn> <JSON params>");
    };
    let service = match service.as_str() {
        "chat" => Service::Chat,
        "group" => Service::Group,
        "conversation" => Service::Conversation,
        "profile" => Service::Profile,
        "friend" => Service::Friend,
        "file" => Service::File,
        "sticker" => Service::Sticker,
        "group_cloud_message" => Service::GroupCloudMessage,
        "reaction" => Service::Reaction,
        "alias" => Service::Alias,
        other => bail!("dịch vụ không hỗ trợ: {other}"),
    };
    let mut params: serde_json::Value =
        serde_json::from_str(params).context("params không phải JSON")?;
    if params.get("imei").is_some_and(|v| v == "auto") {
        params["imei"] = serde_json::json!(client.device().imei);
    }
    match client.get_encrypted(service, path, &params).await {
        Ok(data) => println!("OK {}", shape(&data, 0)),
        Err(e) => println!("LỖI {e}"),
    }
    Ok(())
}

/// Client settings from `getServerInfo` (server configuration, nothing personal):
/// the feature names, and the file-sharing section in full.
async fn limits(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let data = client.server_info().await?;
    // Zalo spells it "setttings"; zca-js falls back to "settings".
    if args.get(1).map(String::as_str) == Some("--services") {
        // Service names and their hosts (public Zalo endpoints).
        if let Some(map) = data.get("zpw_service_map_v3").and_then(|m| m.as_object()) {
            for (name, hosts) in map {
                println!("{name}: {hosts}");
            }
        }
        return Ok(());
    }
    let settings = data.get("setttings").or_else(|| data.get("settings"));
    let Some(features) = settings.and_then(|s| s.get("features")) else {
        println!("không có settings.features; khóa: {}", shape(&data, 2));
        return Ok(());
    };
    let names: Vec<&String> = features
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    println!("features: {names:?}");
    let wanted: Vec<&str> = match args.get(1..) {
        Some(more) if !more.is_empty() => more.iter().map(String::as_str).collect(),
        _ => vec!["sharefile"],
    };
    for key in wanted {
        if let Some(section) = features.get(key) {
            println!("{key}: {}", serde_json::to_string_pretty(section)?);
        }
    }
    Ok(())
}

async fn send(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let (Some(kind), Some(id), Some(text)) = (args.get(1), args.get(2), args.get(3)) else {
        bail!(
            "cần: send <session.json> user|group <id> <nội dung> [--styles JSON] [--urgency 0|1|2]"
        );
    };
    let to = recipient(&client, kind, id)?;
    let mut extras = TextExtras::default();
    let mut flags = args.get(4..).unwrap_or_default().iter();
    while let Some(flag) = flags.next() {
        let value = flags.next().context("thiếu giá trị sau cờ")?;
        match flag.as_str() {
            "--styles" => {
                extras.styles = serde_json::from_str(value).context("--styles không phải JSON")?
            }
            "--urgency" => extras.urgency = value.parse().context("--urgency là 0, 1 hoặc 2")?,
            other => bail!("cờ không hỗ trợ: {other}"),
        }
    }
    let sent = client.send_text_with(to, text, &extras).await?;
    println!("SENT msgId={} clientId={}", sent.msg_id, sent.client_id);
    Ok(())
}

/// Send a contact card; `me` is the account's own.
async fn card(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let (Some(kind), Some(id), Some(uid)) = (args.get(1), args.get(2), args.get(3)) else {
        bail!("cần: card <session.json> user|group <id> <uid | me> [số điện thoại]");
    };
    let to = recipient(&client, kind, id)?;
    let uid = match uid.as_str() {
        "me" => client.session().map(|s| s.uid.clone()).unwrap_or_default(),
        other => other.to_string(),
    };
    let phone = args.get(4).map(String::as_str).unwrap_or("");
    match client.send_contact_card(to, &uid, phone).await {
        Ok(sent) => println!(
            "CARD SENT msgId {} clientId={}",
            if sent.msg_id.is_empty() {
                "trống"
            } else {
                "có"
            },
            sent.client_id
        ),
        Err(e) => println!("LỖI {e}"),
    }
    Ok(())
}

/// Banks Zalo knows (public), saved accounts (counted only), sending one.
async fn bank(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    match args.get(1).map(String::as_str).unwrap_or("banks") {
        "banks" => {
            let banks = client.banks().await?;
            let with_logo = banks.iter().filter(|b| !b.logo.is_empty()).count();
            println!("{} ngân hàng ({with_logo} có logo)", banks.len());
            for b in banks.iter().take(8) {
                println!("  {} {} [{}]", b.bin, b.short_name, b.keywords);
            }
        }
        "accounts" => {
            let accounts = client.bank_accounts().await?;
            let defaults = accounts.iter().filter(|a| a.default).count();
            println!("{} tài khoản đã lưu ({defaults} mặc định)", accounts.len());
        }
        "send" => {
            let (Some(kind), Some(id), Some(bin), Some(number)) =
                (args.get(2), args.get(3), args.get(4), args.get(5))
            else {
                bail!("cần: bank <session.json> send user|group <id> <bin> <số TK> [tên]");
            };
            let to = recipient(&client, kind, id)?;
            let holder = args.get(6).map(String::as_str).unwrap_or("");
            match client.send_bank_card(to, bin, number, holder).await {
                Ok(sent) => println!("BANK CARD SENT clientId={}", sent.client_id),
                Err(e) => println!("LỖI {e}"),
            }
        }
        other => bail!("không hiểu \"{other}\": banks | accounts | send …"),
    }
    Ok(())
}

/// Quick messages: counted, added, removed (a test adds one and removes it).
async fn quick(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    match args.get(1).map(String::as_str).unwrap_or("list") {
        "list" => {
            let items = client.quick_messages().await?;
            let photos = items.iter().filter(|q| !q.photo.is_empty()).count();
            println!("{} tin nhắn nhanh ({photos} có ảnh)", items.len());
        }
        "add" => {
            let (Some(keyword), Some(text)) = (args.get(2), args.get(3)) else {
                bail!("cần: quick <session.json> add <từ khóa> <nội dung>");
            };
            let item = client.add_quick_message(keyword, text).await?;
            println!("ADDED id={} keyword={}", item.id, item.keyword);
        }
        "remove" => {
            let id: u64 = args.get(2).context("thiếu id")?.parse()?;
            client.remove_quick_message(id).await?;
            println!("REMOVED {id}");
        }
        other => bail!("không hiểu \"{other}\": list | add | remove"),
    }
    Ok(())
}

/// A group's board and the reminders of a chat: counts, or one test reminder.
async fn board(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    match args.get(1).map(String::as_str).unwrap_or("") {
        "list" => {
            let group = args.get(2).context("thiếu group id")?;
            let items = client.board(group).await?;
            let count = |f: fn(&BoardItem) -> bool| items.iter().filter(|i| f(i)).count();
            println!(
                "bảng tin: {} ghi chú, {} tin ghim, {} bình chọn",
                count(|i| matches!(i, BoardItem::Note(_))),
                count(|i| matches!(i, BoardItem::Pinned(_))),
                count(|i| matches!(i, BoardItem::Poll(_))),
            );
        }
        "reminders" => {
            let (Some(kind), Some(id)) = (args.get(2), args.get(3)) else {
                bail!("cần: board <session.json> reminders user|group <id>");
            };
            let list = client.reminders(recipient(&client, kind, id)?).await?;
            let repeating = list.iter().filter(|r| r.repeat > 0).count();
            println!("{} nhắc hẹn ({repeating} lặp lại)", list.len());
        }
        "remind" => {
            let (Some(kind), Some(id), Some(title), Some(minutes)) =
                (args.get(2), args.get(3), args.get(4), args.get(5))
            else {
                bail!("cần: board <session.json> remind user|group <id> <tiêu đề> <phút nữa>");
            };
            let minutes: i64 = minutes.parse()?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as i64;
            let reminder = NewReminder {
                title: title.clone(),
                start_at: now + minutes * 60_000,
                ..NewReminder::default()
            };
            match client
                .create_reminder(recipient(&client, kind, id)?, &reminder)
                .await
            {
                Ok(t) => println!("REMINDER id={} start_at={}", t.id, t.start_at),
                Err(e) => println!("LỖI {e}"),
            }
        }
        "unremind" => {
            let (Some(kind), Some(id), Some(reminder)) = (args.get(2), args.get(3), args.get(4))
            else {
                bail!("cần: board <session.json> unremind user|group <id> <reminder id>");
            };
            match client
                .remove_reminder(recipient(&client, kind, id)?, reminder)
                .await
            {
                Ok(()) => println!("REMOVED {reminder}"),
                Err(e) => println!("LỖI {e}"),
            }
        }
        "poll" => {
            let id: u64 = args.get(2).context("thiếu poll id")?.parse()?;
            let p = client.poll(id).await?;
            println!(
                "bình chọn: {} lựa chọn, {} người bầu, đóng: {}",
                p.options.len(),
                p.voters,
                p.closed
            );
        }
        other => bail!("không hiểu \"{other}\": list | reminders | remind | unremind | poll"),
    }
    Ok(())
}

async fn react(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let (Some(kind), Some(id), Some(msg_id), Some(cli_msg_id), Some(icon)) = (
        args.get(1),
        args.get(2),
        args.get(3),
        args.get(4),
        args.get(5),
    ) else {
        bail!("cần: react <session.json> user|group <id> <msgId> <cliMsgId> <biểu tượng>");
    };
    let to = match kind.as_str() {
        "user" => Recipient::User(id),
        "group" => Recipient::Group(id),
        other => bail!("loại người nhận không hợp lệ: {other}"),
    };
    match client.send_reaction(to, msg_id, cli_msg_id, icon).await {
        Ok(sent) => println!(
            "REACTED clientId={} msgIds={:?}",
            sent.client_id, sent.msg_ids
        ),
        Err(e) => println!("LỖI {e}"),
    }
    Ok(())
}

async fn listen(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut rx = socket::spawn(socket::RealtimeConfig::from_client(&client)?);
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            event = rx.recv() => match event {
                None => break,
                Some(RealtimeEvent::Connected { endpoint }) => println!("đã kết nối {endpoint}"),
                Some(RealtimeEvent::Frame(f)) => {
                    let kind = if f.is_message() { "tin nhắn" } else if f.is_control() { "điều khiển" } else { "khác" };
                    println!("cmd={} sub={} ({kind})", f.cmd, f.sub_cmd);
                }
                Some(RealtimeEvent::Disconnected { reason }) => println!("mất kết nối: {reason}"),
                Some(RealtimeEvent::SessionRejected) => {
                    println!("Zalo từ chối phiên, cần đăng nhập lại");
                    break;
                }
            }
        }
    }
    Ok(())
}

fn import_ctdozlite(args: &[String]) -> Result<()> {
    let (Some(src), Some(dst)) = (args.first(), args.get(1)) else {
        bail!("cần: import-ctdozlite <ctdozlite.json> <out.json>");
    };
    let text = std::fs::read_to_string(src).with_context(|| format!("không đọc được {src}"))?;
    let session = SessionData::from_ctdozlite_json(&text)?;
    session.save(Path::new(dst))?;
    println!("Đã chuyển đổi sang {dst}. Kiểm tra lại bằng: zeta check {dst}");
    Ok(())
}

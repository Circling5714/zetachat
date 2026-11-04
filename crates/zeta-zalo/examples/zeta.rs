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
use zeta_zalo::api::{Recipient, SessionHealth};
use zeta_zalo::login::QrEvent;
use zeta_zalo::socket::{self, RealtimeEvent};
use zeta_zalo::{Device, SessionData, ZaloClient};

const USAGE: &str = "\
Cách dùng:
  zeta login   <session.json> [tên máy]          đăng nhập bằng mã QR, lưu phiên
  zeta check   <session.json>                    hỏi Zalo phiên còn dùng được không
  zeta friends <session.json> [--names]          đếm (hoặc liệt kê) bạn bè
  zeta send    <session.json> user|group <id> <nội dung>
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
        Some("send") => send(rest).await,
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

async fn send(args: &[String]) -> Result<()> {
    let client = load_client(&session_path(args)?)?;
    let (Some(kind), Some(id), Some(text)) = (args.get(1), args.get(2), args.get(3)) else {
        bail!("cần: send <session.json> user|group <id> <nội dung>");
    };
    let to = match kind.as_str() {
        "user" => Recipient::User(id),
        "group" => Recipient::Group(id),
        other => bail!("loại người nhận không hợp lệ: {other}"),
    };
    let sent = client.send_text(to, text).await?;
    println!("SENT msgId={} clientId={}", sent.msg_id, sent.client_id);
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

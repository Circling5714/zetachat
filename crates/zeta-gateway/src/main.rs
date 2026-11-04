//! `zetachat-gateway` command line.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use zeta_gateway::api::{self, AppState};
use zeta_gateway::demo;
use zeta_gateway::events::EventBus;
use zeta_gateway::model::now_ms;
use zeta_gateway::store::Store;
use zeta_gateway::zalo::{self, ZaloConfig};
use zeta_gateway::{db_path, session_path};
use zeta_zalo::login::QrEvent;
use zeta_zalo::{Device, SessionData, ZaloClient};

/// Name shown as "Ứng dụng" on the phone when logging in.
const COMPUTER_NAME: &str = "zetachat";

#[derive(Parser)]
#[command(
    name = "zetachat-gateway",
    version,
    about = "zetachat gateway: giữ phiên Zalo và phục vụ các thiết bị của bạn qua LAN/VPN"
)]
struct Cli {
    /// Thư mục dữ liệu (cơ sở dữ liệu, phiên Zalo).
    #[arg(long, env = "ZETACHAT_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Chạy gateway (mặc định).
    Serve {
        /// Địa chỉ lắng nghe. Để 127.0.0.1 nếu reverse proxy chạy cùng máy.
        #[arg(long, env = "ZETACHAT_LISTEN", default_value = "127.0.0.1:8370")]
        listen: SocketAddr,
        /// Dùng dữ liệu mẫu, không kết nối Zalo (để thử giao diện). Cần thư mục dữ liệu riêng.
        #[arg(long)]
        demo: bool,
    },
    /// Quản lý thiết bị được phép dùng API.
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Đăng nhập Zalo bằng mã QR ngay trong terminal (khi gateway đang dừng).
    Login,
    /// Nhập phiên từ file session.json của ctdozlite.
    ImportCtdozlite { path: PathBuf },
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Thêm thiết bị và in token (chỉ hiện một lần).
    Add { name: String },
    /// Liệt kê thiết bị.
    List,
    /// Thu hồi một thiết bị.
    Revoke { id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,zeta_zalo=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    prepare_data_dir(&cli.data_dir)?;
    match cli.command.unwrap_or(Command::Serve {
        listen: "127.0.0.1:8370".parse()?,
        demo: false,
    }) {
        Command::Serve { listen, demo } => serve(&cli.data_dir, listen, demo).await,
        Command::Device { action } => device(&cli.data_dir, action),
        Command::Login => login(&cli.data_dir).await,
        Command::ImportCtdozlite { path } => import_ctdozlite(&cli.data_dir, &path),
    }
}

/// Create the data directory, owner-only on Unix: it holds the Zalo session.
fn prepare_data_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("không tạo được {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Events older than this are dropped at start-up; clients away longer reload.
const EVENT_RETENTION_MS: i64 = 30 * 24 * 3600 * 1000;

async fn serve(data_dir: &Path, listen: SocketAddr, demo: bool) -> Result<()> {
    let store = Arc::new(Store::open(&db_path(data_dir))?);
    match store.prune_events(now_ms() - EVENT_RETENTION_MS) {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "old events pruned"),
        Err(e) => tracing::warn!(error = %e, "cannot prune events"),
    }
    let bus = EventBus::new(store.clone());
    let zalo = if demo {
        if session_path(data_dir).exists() {
            bail!(
                "chế độ demo cần một thư mục dữ liệu riêng, {} đang có phiên Zalo",
                data_dir.display()
            );
        }
        tracing::warn!("demo mode: sample data, Zalo is not contacted");
        demo::spawn(store.clone(), bus.clone())
    } else {
        zalo::spawn(
            ZaloConfig {
                session_path: session_path(data_dir),
                computer_name: COMPUTER_NAME.into(),
            },
            store.clone(),
            bus.clone(),
        )
    };
    let app = api::router(AppState { store, bus, zalo });
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("không mở được cổng {listen}"))?;
    tracing::info!(%listen, "zetachat gateway is listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}

fn device(data_dir: &Path, action: DeviceAction) -> Result<()> {
    let store = Store::open(&db_path(data_dir))?;
    match action {
        DeviceAction::Add { name } => {
            let (device, token) = store.add_device(&name)?;
            println!("Đã thêm thiết bị {} ({})", device.name, device.id);
            println!("Token (chỉ hiện một lần, hãy lưu lại): {token}");
            println!("Hoặc mở trên thiết bị đó: https://<địa chỉ gateway>/#token={token}");
        }
        DeviceAction::List => {
            for d in store.devices()? {
                let state = if d.revoked {
                    "đã thu hồi"
                } else {
                    "đang dùng"
                };
                println!("{}  {:<20} {}", d.id, d.name, state);
            }
        }
        DeviceAction::Revoke { id } => {
            if !store.revoke_device(&id)? {
                bail!("không có thiết bị {id}");
            }
            println!("Đã thu hồi thiết bị {id}");
        }
    }
    Ok(())
}

async fn login(data_dir: &Path) -> Result<()> {
    let path = session_path(data_dir);
    let device = SessionData::load(&path)
        .map(|s| s.device)
        .unwrap_or_else(|_| Device::generate(COMPUTER_NAME));
    let mut client = ZaloClient::new(device)?;
    let session = client
        .login_qr("vi", |event| match event {
            QrEvent::Ticket(t) => {
                match qrcode::QrCode::new(t.payload.as_bytes()) {
                    Ok(code) => println!(
                        "{}",
                        code.render::<qrcode::render::unicode::Dense1x2>()
                            .quiet_zone(true)
                            .build()
                    ),
                    Err(e) => println!("(không vẽ được mã QR: {e})"),
                }
                println!("Quét mã bằng app Zalo trong {} giây.", t.timeout.as_secs());
            }
            QrEvent::Scanned(_) => println!("Đã quét, hãy bấm \"Đăng nhập\" trên điện thoại…"),
            QrEvent::Confirmed => println!("Đã xác nhận, đang tải khóa phiên…"),
        })
        .await?;
    session.save(&path)?;
    println!("Đăng nhập thành công, đã lưu phiên vào {}", path.display());
    Ok(())
}

fn import_ctdozlite(data_dir: &Path, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("không đọc được {}", path.display()))?;
    let session = SessionData::from_ctdozlite_json(&text)?;
    let target = session_path(data_dir);
    session.save(&target)?;
    println!("Đã nhập phiên vào {}", target.display());
    Ok(())
}

//! Browser-facing routes: the embedded client, cookie sessions and the QR image.

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;
use zeta_gateway::api::{self, AppState};
use zeta_gateway::events::EventBus;
use zeta_gateway::media_cache::MediaCache;
use zeta_gateway::model::{thread_id, NewMessage, ThreadKind, ThreadUpsert};
use zeta_gateway::store::Store;
use zeta_gateway::zalo::{LoginState, ZaloHandle, ZaloStatus};

struct Fx {
    app: Router,
    store: Arc<Store>,
    media: Arc<MediaCache>,
    status: Arc<RwLock<ZaloStatus>>,
    token: String,
}

fn fx() -> Fx {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let bus = EventBus::new(store.clone());
    // Nothing listens on the command channel: these tests never send commands.
    let (commands, _) = tokio::sync::mpsc::channel(1);
    let status = Arc::new(RwLock::new(ZaloStatus::default()));
    let zalo = ZaloHandle {
        commands,
        status: status.clone(),
    };
    let (_, token) = store.add_device("web").unwrap();
    let dir = std::env::temp_dir().join(format!("zetachat-web-{:016x}", rand::random::<u64>()));
    let media = Arc::new(MediaCache::new(dir).unwrap());
    let app = api::router(AppState {
        store: store.clone(),
        bus,
        zalo,
        media: media.clone(),
    });
    Fx {
        app,
        store,
        media,
        status,
        token,
    }
}

async fn send(f: &Fx, req: Request<Body>) -> Response<Body> {
    f.app.clone().oneshot(req).await.unwrap()
}

async fn body_text(resp: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn get(uri: &str) -> axum::http::request::Builder {
    Request::builder().method("GET").uri(uri)
}

/// Percent-encoding for a query value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn body_bytes(resp: Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn media_comes_from_the_cache_with_safe_headers() {
    let f = fx();
    let auth = format!("Bearer {}", f.token);
    let request = |url: &str, authorized: bool, range: Option<&str>| {
        let mut req = get(&format!("/api/v1/media?u={}", encode(url)));
        if authorized {
            req = req.header(header::AUTHORIZATION, &auth);
        }
        if let Some(range) = range {
            req = req.header(header::RANGE, range);
        }
        req.body(Body::empty()).unwrap()
    };

    let photo = "https://photo-stal-1.zdn.vn/jpg/a.jpg?sig=1";
    let mut jpeg = b"\xFF\xD8\xFF\xE0".to_vec();
    jpeg.extend([7u8; 96]);
    // Zalo labels its photos `image/jpg`.
    f.media.insert(photo, &jpeg, "image/jpg").await.unwrap();

    let resp = send(&f, request(photo, false, None)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    for foreign in [
        "https://example.com/a.jpg",
        "http://127.0.0.1:8370/api/v1/status",
        "file:///etc/passwd",
    ] {
        let resp = send(&f, request(foreign, true, None)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{foreign}");
    }

    let resp = send(&f, request(photo, true, None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        headers["content-security-policy"],
        "default-src 'none'; sandbox"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(headers[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .starts_with("private"));
    assert!(headers.get(header::CONTENT_DISPOSITION).is_none());
    assert_eq!(body_bytes(resp).await, jpeg);

    // Ranges, for seeking in videos.
    let resp = send(&f, request(photo, true, Some("bytes=0-3"))).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 0-3/100");
    assert_eq!(body_bytes(resp).await, b"\xFF\xD8\xFF\xE0");

    // A page sent as a file must not run in the gateway's origin.
    let page = "https://f1.zdn.vn/files/page.html";
    f.media
        .insert(page, b"<html><script>alert(1)</script>", "text/html")
        .await
        .unwrap();
    let resp = send(&f, request(page, true, None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(resp.headers()[header::CONTENT_DISPOSITION], "attachment");

    std::fs::remove_dir_all(f.media.dir()).unwrap();
}

#[tokio::test]
async fn client_is_served_with_a_strict_content_security_policy() {
    let f = fx();
    let resp = send(&f, get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let csp = resp.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    // Media comes through the gateway: the page loads nothing from other hosts.
    assert!(csp.contains("img-src 'self' data:;"), "{csp}");
    assert!(csp.contains("media-src 'self'"), "{csp}");
    assert!(!csp.contains("https:"), "{csp}");
    assert_eq!(resp.headers()["referrer-policy"], "no-referrer");
    assert!(body_text(resp).await.contains("<title>zetachat</title>"));

    for (path, kind) in [
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
        ("/sw.js", "text/javascript"),
        ("/manifest.webmanifest", "application/manifest+json"),
        ("/icon.svg", "image/svg+xml"),
    ] {
        let resp = send(&f, get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        let content_type = resp.headers()[header::CONTENT_TYPE].to_str().unwrap();
        assert!(content_type.starts_with(kind), "{path}: {content_type}");
    }
}

#[tokio::test]
async fn token_is_exchanged_for_an_http_only_cookie() {
    let f = fx();
    let login = |token: &str, https: bool| {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/v1/session")
            .header(header::CONTENT_TYPE, "application/json");
        if https {
            req = req.header("x-forwarded-proto", "https");
        }
        req.body(Body::from(json!({ "token": token }).to_string()))
            .unwrap()
    };

    let resp = send(&f, login("zc_wrong", true)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = send(&f, login(&f.token, true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    for part in [
        "zetachat_token=zc_",
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/",
    ] {
        assert!(cookie.contains(part), "{part} missing in {cookie}");
    }
    let plain = send(&f, login(&f.token, false)).await;
    let plain_cookie = plain.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(!plain_cookie.contains("Secure"));

    // The cookie alone authenticates.
    let pair = cookie.split(';').next().unwrap();
    let resp = send(
        &f,
        get("/api/v1/status")
            .header(header::COOKIE, pair)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(
        &f,
        Request::builder()
            .method("DELETE")
            .uri("/api/v1/session")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let cleared = resp.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
}

#[tokio::test]
async fn login_qr_image_exists_only_while_waiting_for_a_scan() {
    let f = fx();
    let qr_request = || {
        get("/api/v1/zalo/login/qr.svg")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.token))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send(&f, qr_request()).await.status(), StatusCode::NOT_FOUND);

    f.status.write().unwrap().login = LoginState::WaitingScan {
        qr: "https://example.invalid/qr".into(),
        expires_at: 0,
    };
    let resp = send(&f, qr_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert!(body_text(resp).await.contains("<svg"));
}

#[tokio::test]
async fn threads_carry_a_preview_and_status_the_last_event() {
    let f = fx();
    f.store
        .upsert_thread(
            &ThreadUpsert {
                kind: ThreadKind::User,
                remote_id: "5".into(),
                name: "C".into(),
                avatar: String::new(),
            },
            true,
        )
        .unwrap();
    let id = thread_id(ThreadKind::User, "5");
    for (i, text) in ["một", "hai"].iter().enumerate() {
        f.store
            .insert_message(&NewMessage {
                thread_id: id.clone(),
                remote_id: format!("m{i}"),
                client_id: String::new(),
                sender_id: "5".into(),
                sender_name: "C".into(),
                outgoing: false,
                kind: "webchat".into(),
                text: text.to_string(),
                media: None,
                sent_at: i as i64 + 1,
                raw: "{}".into(),
            })
            .unwrap();
    }
    f.store.append_event("x", &json!({}), 1).unwrap();

    let auth = format!("Bearer {}", f.token);
    let resp = send(
        &f,
        get("/api/v1/threads")
            .header(header::AUTHORIZATION, &auth)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let threads: Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(threads[0]["preview"], "hai");

    let resp = send(
        &f,
        get("/api/v1/status")
            .header(header::AUTHORIZATION, &auth)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status: Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(status["last_event_seq"], 1);
}

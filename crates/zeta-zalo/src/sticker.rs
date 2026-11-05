//! Stickers, as ctdozlite does it after Zalo PC: the packs of Zalo's sticker
//! store, the stickers of a pack, a search by keyword, and sending one.
//!
//! - Packs: `GET {sticker}/api/message/sticker/category/list/v2`, whose params
//!   are the encrypted empty string, answers `dataUrl`: the catalogue on Zalo's
//!   CDN (JSON, sometimes gzipped). `exCateIds` lists packs to leave out.
//! - A pack: `GET {sticker}/api/message/sticker/category/sticker_detail` with
//!   `{cid}`.
//! - Search: `GET {sticker}/api/message/sticker/suggest/stickers` (`sugg_sticker`)
//!   and `/api/message/sticker/search` (a list), merged.
//! - Send: `POST {chat}/api/message/sticker` or `{group}/api/group/sticker` with
//!   `stickerId`, `cateId`, `type`, `clientId`, `imei` and `zsource: 101`.
//!
//! Pictures: `https://api.zaloapp.com/api/emoticon/sticker/webpc?eid=<id>&size=130`,
//! or a fixed URL for the 30 built-in stickers (pack 0).

use std::io::Read;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::api::{first_str, Recipient, SentMessage};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::Result;
use crate::now_millis;

/// The catalogue of packs when `category/list/v2` does not say where it is.
const CATALOGUE_URL: &str = "https://zalo-api.zadn.vn/api/emoticon/pcweb/getAllCate";

/// The catalogue is a few hundred kilobytes; anything far bigger is not it.
const CATALOGUE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// One pack of the sticker store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerPack {
    pub id: u64,
    pub name: String,
    /// Picture of the pack on Zalo's CDN; may be empty.
    pub icon: String,
    /// Stickers in it, as the catalogue says.
    pub count: u32,
}

/// One sticker: what sending it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    pub id: u64,
    /// Its pack (`cateId`); 0 for Zalo's 30 built-in stickers.
    pub cate_id: u64,
    /// Zalo's `type` (7 for most store stickers).
    #[serde(rename = "type")]
    pub kind: u32,
}

impl Sticker {
    /// The sticker's picture on Zalo's CDN (the gateway's message parsing uses
    /// the same rule).
    pub fn url(&self) -> String {
        if self.cate_id == 0 && (1..=30).contains(&self.id) {
            format!(
                "https://stc-chat.zdn.vn/images/stickers/default/thumb/{}.png",
                self.id
            )
        } else {
            format!(
                "https://api.zaloapp.com/api/emoticon/sticker/webpc?eid={}&size=130",
                self.id
            )
        }
    }
}

/// How a sticker moves: Zalo draws animated stickers from a sprite sheet, the
/// frames side by side, shown one after the other over `duration_ms`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerMotion {
    /// The sprite sheet on Zalo's CDN.
    pub sprite: String,
    /// An animated WebP, when Zalo has one (it often has not).
    pub webp: String,
    pub frames: u32,
    pub duration_ms: u32,
}

/// The motion in a `sticker_detail` answer; `None` for a still sticker.
fn motion_of(data: &Value) -> Option<StickerMotion> {
    let url = |key: &str| first_str(data, &[key]).replace("http://", "https://");
    let motion = StickerMotion {
        sprite: url("stickerSpriteUrl"),
        webp: url("stickerWebpUrl"),
        frames: u32::try_from(number(data, &["totalFrames"])).unwrap_or(0),
        duration_ms: u32::try_from(number(data, &["duration"])).unwrap_or(0),
    };
    let moves = (!motion.sprite.is_empty() && motion.frames > 1) || !motion.webp.is_empty();
    moves.then_some(motion)
}

fn number(v: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| match v.get(*k) {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

/// A sticker from any of the answers; `pack` when the item does not say.
fn sticker_of(v: &Value, pack: u64) -> Option<Sticker> {
    let id = number(v, &["sticker_id", "stickerId", "id"]);
    (id > 0).then(|| Sticker {
        id,
        cate_id: match number(v, &["cate_id", "cateId", "catId"]) {
            0 => pack,
            cate => cate,
        },
        kind: u32::try_from(number(v, &["type"]))
            .ok()
            .filter(|t| *t > 0)
            .unwrap_or(7),
    })
}

/// A list of items: the value itself, or the first list under these keys.
fn items<'a>(v: &'a Value, keys: &[&str]) -> &'a [Value] {
    if let Some(list) = v.as_array() {
        return list;
    }
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

/// Packs of the catalogue: named, not excluded, not test packs; newest first.
fn packs_of(catalogue: &Value, excluded: &[u64]) -> Vec<StickerPack> {
    let mut packs: Vec<StickerPack> = items(catalogue, &["data", "cates", "listCate"])
        .iter()
        .filter_map(|item| {
            let id = number(item, &["id", "cateId"]);
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if id == 0 || excluded.contains(&id) || name.is_empty() {
                return None;
            }
            if name.to_lowercase().contains("test") {
                return None;
            }
            let icon =
                first_str(item, &["iconUrl", "thumbImg", "icon"]).replace("http://", "https://");
            Some(StickerPack {
                id,
                name,
                icon,
                count: u32::try_from(number(item, &["totalImage", "total"])).unwrap_or(0),
            })
        })
        .collect();
    packs.sort_by_key(|p| std::cmp::Reverse(p.id));
    packs.dedup_by_key(|p| p.id);
    packs
}

/// Search answers: the suggestions first, then the other matches, once each.
fn merge_found(suggested: Option<&Value>, searched: Option<&Value>) -> Vec<Sticker> {
    let mut out: Vec<Sticker> = Vec::new();
    let lists = [
        suggested.map(|v| items(v, &["sugg_sticker"])),
        searched.map(|v| items(v, &["data", "stickers"])),
    ];
    for item in lists.into_iter().flatten().flatten() {
        if let Some(s) = sticker_of(item, 0) {
            if !out.iter().any(|o| o.id == s.id) {
                out.push(s);
            }
        }
    }
    out
}

fn gunzip_if_needed(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes);
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .take(CATALOGUE_MAX_BYTES * 4)
        .read_to_end(&mut out)?;
    Ok(out)
}

impl ZaloClient {
    /// The packs of Zalo's sticker store, newest first.
    pub async fn sticker_packs(&self) -> Result<Vec<StickerPack>> {
        let key = self.session_key()?;
        let path = format!(
            "/api/message/sticker/category/list/v2?{}&params={}",
            Self::common_params(),
            self.encrypt_text("")?
        );
        let data = self.get_service(Service::Sticker, &path, Some(key)).await?;
        let excluded: Vec<u64> = data
            .get("exCateIds")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        let url = data
            .get("dataUrl")
            .and_then(Value::as_str)
            .map(|u| u.trim().replace("http://", "https://"))
            .filter(|u| crate::media::is_media_url(u))
            .unwrap_or_else(|| CATALOGUE_URL.to_string());
        let bytes = gunzip_if_needed(self.get_bytes(&url, CATALOGUE_MAX_BYTES).await?)?;
        let catalogue: Value = serde_json::from_slice(&bytes)?;
        Ok(packs_of(&catalogue, &excluded))
    }

    /// How stickers move (`sticker_detail`, one call per sticker as Zalo PC
    /// does): their sprite sheet, frame count and duration.
    pub async fn sticker_motion(&self, id: u64) -> Result<Option<StickerMotion>> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "sid": id }))?;
        let path = format!(
            "/api/message/sticker/sticker_detail?{}&params={params}",
            Self::common_params()
        );
        let data = self.get_service(Service::Sticker, &path, Some(key)).await?;
        Ok(motion_of(&data))
    }

    /// The stickers of one pack.
    pub async fn pack_stickers(&self, cate_id: u64) -> Result<Vec<Sticker>> {
        let key = self.session_key()?;
        let params = self.encrypt_params(&json!({ "cid": cate_id.to_string() }))?;
        let path = format!(
            "/api/message/sticker/category/sticker_detail?{}&params={params}",
            Self::common_params()
        );
        let data = self.get_service(Service::Sticker, &path, Some(key)).await?;
        Ok(items(&data, &["data", "stickers"])
            .iter()
            .filter_map(|v| sticker_of(v, cate_id))
            .collect())
    }

    /// Stickers for a keyword ("haha", "yêu", "cảm ơn"…): Zalo's suggestions,
    /// then its search.
    pub async fn search_stickers(&self, keyword: &str) -> Result<Vec<Sticker>> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }
        let key = self.session_key()?;
        let imei = &self.device().imei;
        let common = Self::common_params();
        let suggest = format!(
            "/api/message/sticker/suggest/stickers?{common}&params={}",
            self.encrypt_params(
                &json!({ "keyword": keyword, "gif": 1, "guggy": 0, "imei": imei })
            )?
        );
        let search = format!(
            "/api/message/sticker/search?{common}&params={}",
            self.encrypt_params(
                &json!({ "keyword": keyword, "limit": 50, "srcType": 0, "imei": imei })
            )?
        );
        let (suggested, searched) = tokio::join!(
            self.get_service(Service::Sticker, &suggest, Some(key)),
            self.get_service(Service::Sticker, &search, Some(key)),
        );
        let found = merge_found(suggested.as_ref().ok(), searched.as_ref().ok());
        match (suggested, searched) {
            (Err(e), Err(_)) if found.is_empty() => Err(e),
            _ => Ok(found),
        }
    }

    /// Send a sticker.
    pub async fn send_sticker(&self, to: Recipient<'_>, sticker: &Sticker) -> Result<SentMessage> {
        let key = self.session_key()?;
        let client_id = now_millis();
        let mut p = Map::new();
        let (service, path) = match to {
            Recipient::User(uid) => {
                p.insert("toid".into(), json!(uid));
                (Service::Chat, "/api/message/sticker")
            }
            Recipient::Group(grid) => {
                p.insert("grid".into(), json!(grid));
                (Service::Group, "/api/group/sticker")
            }
        };
        p.insert("stickerId".into(), json!(sticker.id));
        p.insert("cateId".into(), json!(sticker.cate_id));
        p.insert("type".into(), json!(sticker.kind));
        p.insert("clientId".into(), json!(client_id));
        p.insert("imei".into(), json!(self.device().imei));
        p.insert("zsource".into(), json!(101));
        let form = format!("params={}", self.encrypt_params(&Value::Object(p))?);
        let data = self.post_service(service, path, form, Some(key)).await?;
        Ok(SentMessage {
            msg_id: first_str(&data, &["msgId"]),
            client_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_skip_excluded_test_and_nameless_ones() {
        let catalogue = json!([
            { "id": 10, "name": "Mèo", "totalImage": 24, "iconUrl": "http://zalo-api.zadn.vn/10.png" },
            { "id": 12, "name": "Gấu", "thumbImg": "https://zalo-api.zadn.vn/12.png" },
            { "id": 11, "name": "Test pack" },
            { "id": 13, "name": "  " },
            { "id": 14, "name": "Bỏ" },
            { "name": "Không id" }
        ]);
        let packs = packs_of(&catalogue, &[14]);
        let ids: Vec<u64> = packs.iter().map(|p| p.id).collect();
        assert_eq!(ids, [12, 10]);
        assert_eq!(packs[1].icon, "https://zalo-api.zadn.vn/10.png");
        assert_eq!((packs[1].count, packs[0].count), (24, 0));
        // The catalogue can also come wrapped.
        assert_eq!(packs_of(&json!({ "data": catalogue }), &[]).len(), 3);
    }

    #[test]
    fn stickers_of_a_pack_and_of_a_search() {
        let pack = json!([{ "id": 501, "cateId": 10, "type": 7 }, { "id": 502 }, { "id": 0 }]);
        let found: Vec<Sticker> = items(&pack, &["data"])
            .iter()
            .filter_map(|v| sticker_of(v, 10))
            .collect();
        assert_eq!(
            found,
            [
                Sticker {
                    id: 501,
                    cate_id: 10,
                    kind: 7
                },
                Sticker {
                    id: 502,
                    cate_id: 10,
                    kind: 7
                }
            ]
        );
        let suggested = json!({ "sugg_sticker": [{ "sticker_id": 7, "cate_id": 3, "type": 1 }] });
        let searched =
            json!([{ "sticker_id": 8, "cate_id": 3 }, { "sticker_id": 7, "cate_id": 3 }]);
        let merged = merge_found(Some(&suggested), Some(&searched));
        assert_eq!(merged.iter().map(|s| s.id).collect::<Vec<_>>(), [7, 8]);
        assert_eq!(merged[0].kind, 1);
        assert!(merge_found(None, None).is_empty());
    }

    #[test]
    fn sticker_motion() {
        let animated = json!({
            "id": 101001, "stickerUrl": "https://zalo-api.zadn.vn/s.png",
            "stickerSpriteUrl": "http://zalo-api.zadn.vn/sprite.png", "stickerWebpUrl": null,
            "totalFrames": 12, "duration": 1200
        });
        assert_eq!(
            motion_of(&animated),
            Some(StickerMotion {
                sprite: "https://zalo-api.zadn.vn/sprite.png".into(),
                webp: String::new(),
                frames: 12,
                duration_ms: 1200,
            })
        );
        let still = json!({ "stickerSpriteUrl": "https://x/s.png", "totalFrames": 1 });
        assert_eq!(motion_of(&still), None);
    }

    #[test]
    fn sticker_pictures() {
        let builtin = Sticker {
            id: 7,
            cate_id: 0,
            kind: 1,
        };
        assert!(builtin.url().ends_with("/default/thumb/7.png"));
        let store = Sticker {
            id: 21300,
            cate_id: 10,
            kind: 7,
        };
        assert!(store.url().ends_with("eid=21300&size=130"));
        assert!(crate::media::is_media_url(&store.url()));
    }

    #[test]
    fn gzipped_catalogues_are_unpacked() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"[1,2]").unwrap();
        let packed = gz.finish().unwrap();
        assert_eq!(gunzip_if_needed(packed).unwrap(), b"[1,2]");
        assert_eq!(gunzip_if_needed(b"[3]".to_vec()).unwrap(), b"[3]");
    }
}

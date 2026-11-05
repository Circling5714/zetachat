//! Answers of Zalo's sticker store, kept for a while: the catalogue of packs
//! changes rarely, a pack never, searches a little. Clients get stickers with
//! their picture's URL, to load through the media proxy.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zeta_zalo::sticker::{Sticker, StickerMotion, StickerPack};
use zeta_zalo::ZaloClient;

use crate::store::fold;

/// How long the catalogue of packs is kept.
const PACKS_FRESH: Duration = Duration::from_secs(12 * 3600);
/// How long a search's answer is kept.
const SEARCH_FRESH: Duration = Duration::from_secs(3600);
/// Packs and searches kept at most; the oldest go first.
const MAX_KEPT: usize = 200;

/// What a client asks of the sticker store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StickerQuery {
    Packs,
    Pack(u64),
    Search(String),
    /// How these stickers move (at most 50).
    Motion(Vec<u64>),
}

/// Motions kept at most; stickers never change, so only the count matters.
const MAX_MOTIONS: usize = 5000;

/// A sticker's motion for clients: `{ sprite, frames, duration_ms }` (per
/// frame), `null` for a still one.
fn motion_json(m: Option<&StickerMotion>) -> Value {
    match m {
        Some(m) if m.frames > 1 => json!({
            "sprite": m.sprite,
            "frames": m.frames,
            "duration_ms": if m.duration_ms > 0 { m.duration_ms } else { 100 },
        }),
        _ => Value::Null,
    }
}

/// A sticker for clients: what sending it takes, and its picture.
pub fn sticker_json(s: &Sticker) -> Value {
    json!({ "id": s.id, "cate_id": s.cate_id, "type": s.kind, "url": s.url() })
}

#[derive(Default)]
pub struct StickerCache {
    packs: Mutex<Option<(Instant, Vec<StickerPack>)>>,
    pack: Mutex<HashMap<u64, (Instant, Vec<Sticker>)>>,
    search: Mutex<HashMap<String, (Instant, Vec<Sticker>)>>,
    motion: Mutex<HashMap<u64, Option<StickerMotion>>>,
}

/// Keep `value` under `key`, dropping the oldest entry when full.
fn keep<K: Clone + Eq + std::hash::Hash, V>(map: &mut HashMap<K, (Instant, V)>, key: K, value: V) {
    if map.len() >= MAX_KEPT && !map.contains_key(&key) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(key, (Instant::now(), value));
}

impl StickerCache {
    /// The answer to `query`: from what is kept, or from Zalo.
    pub async fn answer(&self, client: &ZaloClient, query: &StickerQuery) -> Result<Value, String> {
        match query {
            StickerQuery::Packs => {
                let kept = self.packs.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let packs = match kept {
                    Some((at, packs)) if at.elapsed() < PACKS_FRESH => packs,
                    _ => {
                        let packs = client.sticker_packs().await.map_err(|e| e.to_string())?;
                        *self.packs.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some((Instant::now(), packs.clone()));
                        packs
                    }
                };
                Ok(json!(packs))
            }
            StickerQuery::Pack(id) => {
                let kept = self
                    .pack
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(id)
                    .map(|(_, list)| list.clone());
                let list = match kept {
                    Some(list) => list,
                    None => {
                        let list = client.pack_stickers(*id).await.map_err(|e| e.to_string())?;
                        if !list.is_empty() {
                            let mut map = self.pack.lock().unwrap_or_else(|e| e.into_inner());
                            keep(&mut map, *id, list.clone());
                        }
                        list
                    }
                };
                Ok(Value::Array(list.iter().map(sticker_json).collect()))
            }
            StickerQuery::Motion(ids) => {
                let mut answer = serde_json::Map::new();
                for id in ids.iter().take(50) {
                    let kept = self
                        .motion
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(id)
                        .cloned();
                    let motion = match kept {
                        Some(motion) => motion,
                        None => {
                            // A failed question is not kept: asked again next time.
                            let Ok(motion) = client.sticker_motion(*id).await else {
                                answer.insert(id.to_string(), Value::Null);
                                continue;
                            };
                            let mut map = self.motion.lock().unwrap_or_else(|e| e.into_inner());
                            if map.len() >= MAX_MOTIONS {
                                map.clear();
                            }
                            map.insert(*id, motion.clone());
                            motion
                        }
                    };
                    answer.insert(id.to_string(), motion_json(motion.as_ref()));
                }
                Ok(Value::Object(answer))
            }
            StickerQuery::Search(keyword) => {
                let key = fold(keyword.trim());
                let kept = self
                    .search
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&key)
                    .filter(|(at, _)| at.elapsed() < SEARCH_FRESH)
                    .map(|(_, list)| list.clone());
                let list = match kept {
                    Some(list) => list,
                    None => {
                        let list = client
                            .search_stickers(keyword.trim())
                            .await
                            .map_err(|e| e.to_string())?;
                        let mut map = self.search.lock().unwrap_or_else(|e| e.into_inner());
                        keep(&mut map, key, list.clone());
                        list
                    }
                };
                Ok(Value::Array(list.iter().map(sticker_json).collect()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_entry_goes_when_full() {
        let mut map: HashMap<u64, (Instant, u64)> = HashMap::new();
        for i in 0..MAX_KEPT as u64 {
            keep(&mut map, i, i);
        }
        keep(&mut map, 1000, 1000);
        assert_eq!(map.len(), MAX_KEPT);
        assert!(map.contains_key(&1000) && !map.contains_key(&0));
        // Replacing a kept entry drops nothing.
        keep(&mut map, 1000, 1);
        assert_eq!(map.len(), MAX_KEPT);
    }

    #[test]
    fn motions_for_clients() {
        let moving = StickerMotion {
            sprite: "https://zalo-api.zadn.vn/api/emoticon/sprite?eid=1&size=130".into(),
            webp: String::new(),
            frames: 10,
            duration_ms: 0,
        };
        assert_eq!(motion_json(Some(&moving))["duration_ms"], 100);
        assert_eq!(motion_json(Some(&moving))["frames"], 10);
        assert!(motion_json(None).is_null());
    }

    #[test]
    fn stickers_carry_their_picture() {
        let s = Sticker {
            id: 21300,
            cate_id: 10,
            kind: 7,
        };
        let v = sticker_json(&s);
        assert_eq!(
            (v["id"].as_u64(), v["type"].as_u64()),
            (Some(21300), Some(7))
        );
        assert!(v["url"].as_str().unwrap().ends_with("eid=21300&size=130"));
    }
}

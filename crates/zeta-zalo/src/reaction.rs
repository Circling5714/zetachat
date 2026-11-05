//! Reactions: the emoji under a message.
//!
//! Sending follows zca-js `addReaction`: `POST {reaction}/api/{message|group}/reaction`
//! with `react_list: [{ message, clientId }]`, where `message` is the JSON text of
//! `{ rMsg: [{ gMsgID, cMsgID, msgType: 1 }], rIcon, rType, source: 6 }`, plus
//! `toid`, or `grid` and `imei`. An empty `rIcon` (`rType` -1) takes back the
//! account's reactions to that message.
//!
//! Frames `612` push reactions as they happen; `610` and `611` (sub 1) carry the
//! ones missed while offline. Each has lists `reacts` (1-1) and `reactGroups`
//! whose items look like messages: `msgId`, `cliMsgId`, `uidFrom`, `idTo`,
//! `dName`, `ts` and a `content` (often JSON text) holding `rMsg`, `rIcon`.

use serde_json::{json, Map, Value};

use crate::api::{first_str, Recipient};
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::{Error, Result};
use crate::now_millis;

/// The six reactions Zalo offers, with their `rType`
/// (`setttings.features.reaction_setting.reactionMsgInfo`, November 2026).
pub const DEFAULT_REACTIONS: [(&str, i64); 6] = [
    ("/-strong", 3),
    ("/-heart", 5),
    (":>", 0),
    (":o", 32),
    (":-((", 2),
    (":-h", 20),
];

/// `rType` of an icon this crate can send; `-1` for the empty icon (take back).
pub fn reaction_type(icon: &str) -> Option<i64> {
    if icon.is_empty() {
        return Some(-1);
    }
    DEFAULT_REACTIONS
        .iter()
        .find(|(code, _)| *code == icon)
        .map(|(_, rtype)| *rtype)
}

/// The emoji Zalo draws for an icon code (zca-js `Reactions`), if known.
pub fn reaction_emoji(icon: &str) -> Option<&'static str> {
    Some(match icon {
        "/-strong" => "👍",
        "/-heart" => "❤️",
        ":>" => "😆",
        ":o" => "😮",
        ":-((" => "😭",
        ":-h" => "😡",
        ":-*" => "😘",
        ":')" => "😂",
        "/-shit" => "💩",
        "/-rose" => "🌹",
        "/-break" => "💔",
        "/-weak" => "👎",
        ";xx" => "😍",
        ";-/" => "😕",
        ";-)" => "😉",
        "/-fade" => "🥀",
        "/-li" => "☀️",
        "/-bd" => "🎂",
        "/-bome" => "💣",
        "/-ok" => "👌",
        "/-v" => "✌️",
        "/-thanks" => "🙏",
        "/-punch" => "👊",
        "_()_" => "🙏",
        "/-no" => "🙅",
        "/-loveu" => "🤟",
        "--b" => "😓",
        ":((" => "😢",
        "x-)" => "😎",
        "8-)" => "🤓",
        ";-d" => "😁",
        "b-)" => "😎",
        ":--|" => "😐",
        "p-(" => "😞",
        ":-bye" => "👋",
        "|-)" => "😴",
        ":wipe" => "😅",
        "&-(" => "😧",
        ":handclap" => "👏",
        ">-|" => "😠",
        ";-x" => "🤐",
        ":-o" => "😲",
        ";-s" => "😳",
        ";-a" => "😨",
        ":-<" => "😟",
        ":))" => "😄",
        "$-)" => "🤑",
        "/-beer" => "🍺",
        _ => return None,
    })
}

/// A reaction found in a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    /// Id of the reaction itself (`msgId`, else `actionId`).
    pub action_id: String,
    /// `cliMsgId` of the reaction: the `clientId` its sender chose.
    pub client_id: String,
    /// Who reacted; `"0"` is this account.
    pub uid_from: String,
    /// The other user, or the group.
    pub id_to: String,
    /// From `reactGroups`.
    pub group: bool,
    pub sender_name: String,
    /// The messages reacted to (one reaction can cover several, seen live):
    /// global id and client id of each.
    pub targets: Vec<(String, String)>,
    /// Icon code; empty when the sender took back their reactions.
    pub icon: String,
    /// Milliseconds since the epoch.
    pub ts: i64,
}

fn parse_reaction(item: &Value, group: bool) -> Option<Reaction> {
    let content = match item.get("content") {
        Some(Value::String(text)) => serde_json::from_str(text).ok()?,
        Some(other) => other.clone(),
        None => return None,
    };
    let targets: Vec<(String, String)> = content
        .get("rMsg")?
        .as_array()?
        .iter()
        .map(|t| {
            (
                first_str(t, &["gMsgID", "gMsgId"]),
                first_str(t, &["cMsgID", "cMsgId"]),
            )
        })
        .filter(|(global, client)| !global.is_empty() || !client.is_empty())
        .collect();
    let action_id = first_str(item, &["msgId", "actionId"]);
    if action_id.is_empty() || targets.is_empty() {
        return None;
    }
    Some(Reaction {
        action_id,
        client_id: first_str(item, &["cliMsgId"]),
        uid_from: first_str(item, &["uidFrom"]),
        id_to: first_str(item, &["idTo"]),
        group,
        sender_name: first_str(item, &["dName"]),
        targets,
        icon: content
            .get("rIcon")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ts: first_str(item, &["ts"]).parse().unwrap_or(0),
    })
}

/// The reactions in a real-time frame payload (`reacts`, `reactGroups`).
pub fn frame_reactions(payload: &Value) -> Vec<Reaction> {
    let data = payload
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(payload);
    let mut out = Vec::new();
    for (key, group) in [("reacts", false), ("reactGroups", true)] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            out.extend(items.iter().filter_map(|it| parse_reaction(it, group)));
        }
    }
    out
}

/// A reaction Zalo accepted.
#[derive(Clone, Debug)]
pub struct SentReaction {
    /// The `clientId` sent; Zalo's echo carries it as `cliMsgId`.
    pub client_id: u64,
    /// Ids Zalo gave the reaction, if it said.
    pub msg_ids: Vec<String>,
}

/// `msgIds` of an answer: an array, or the JSON text of one.
fn answer_ids(data: &Value) -> Vec<String> {
    let ids = match data.get("msgIds") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    ids.as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| match id {
                    Value::Number(n) => Some(n.to_string()),
                    Value::String(s) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Ids are numbers in the official client; keep anything else as a string.
fn id_number(id: &str) -> Value {
    id.parse::<u64>()
        .map(Value::from)
        .unwrap_or_else(|_| json!(id))
}

impl ZaloClient {
    /// React with `icon` to the message `msg_id` / `cli_msg_id`; an empty icon
    /// takes back this account's reactions to it.
    pub async fn send_reaction(
        &self,
        to: Recipient<'_>,
        msg_id: &str,
        cli_msg_id: &str,
        icon: &str,
    ) -> Result<SentReaction> {
        let key = self.session_key()?;
        let rtype = reaction_type(icon)
            .ok_or_else(|| Error::Protocol(format!("unsupported reaction {icon:?}")))?;
        let client_id = now_millis();
        let message = json!({
            "rMsg": [{
                "gMsgID": id_number(msg_id),
                "cMsgID": id_number(cli_msg_id),
                "msgType": 1,
            }],
            "rIcon": icon,
            "rType": rtype,
            "source": 6,
        });
        let mut params = Map::new();
        params.insert(
            "react_list".into(),
            json!([{ "message": message.to_string(), "clientId": client_id }]),
        );
        let scope = match to {
            Recipient::User(uid) => {
                params.insert("toid".into(), json!(uid));
                "message"
            }
            Recipient::Group(grid) => {
                params.insert("grid".into(), json!(grid));
                params.insert("imei".into(), json!(self.device().imei));
                "group"
            }
        };
        let url = format!(
            "{}/api/{scope}/reaction?{}",
            self.domains().primary(Service::Reaction),
            Self::common_params()
        );
        let form = format!("params={}", self.encrypt_params(&Value::Object(params))?);
        let data = self.post_form(&url, form, Some(key)).await?;
        tracing::debug!(shape = %crate::socket::payload_shape(&data), "reaction answer");
        Ok(SentReaction {
            client_id,
            msg_ids: answer_ids(&data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reactions_are_read_from_both_lists() {
        let payload = json!({ "data": {
            "reacts": [{
                "msgId": "9001", "cliMsgId": "77", "uidFrom": "100", "idTo": "0", "dName": "Lan",
                "ts": "1700000000000",
                "content": "{\"rMsg\":[{\"gMsgID\":5005,\"cMsgID\":6006,\"msgType\":1}],\"rIcon\":\"/-heart\",\"rType\":5,\"source\":6}"
            }, { "msgId": "x", "content": "{}" }],
            "reactGroups": [{
                "actionId": "9002", "uidFrom": "0", "idTo": "333",
                "content": { "rMsg": [{ "gMsgID": "5006", "cMsgID": "0" }, { "gMsgID": "5007" }],
                             "rIcon": "", "rType": -1 }
            }]
        }});
        let got = frame_reactions(&payload);
        assert_eq!(got.len(), 2);
        assert_eq!(
            (
                got[0].action_id.as_str(),
                got[0].client_id.as_str(),
                got[0].icon.as_str()
            ),
            ("9001", "77", "/-heart")
        );
        assert_eq!(got[0].targets, [("5005".to_string(), "6006".to_string())]);
        assert_eq!(
            (got[0].ts, got[0].group, got[0].sender_name.as_str()),
            (1_700_000_000_000, false, "Lan")
        );
        assert_eq!((got[1].action_id.as_str(), got[1].group), ("9002", true));
        assert_eq!(got[1].targets.len(), 2, "one reaction, two messages");
        assert_eq!(got[1].icon, "", "taken back");
        assert!(frame_reactions(&json!({ "data": { "msgs": [] } })).is_empty());
    }

    #[test]
    fn icons_types_and_emoji() {
        assert_eq!(reaction_type("/-heart"), Some(5));
        assert_eq!(reaction_type(""), Some(-1));
        assert_eq!(
            reaction_type("/-beer"),
            None,
            "only the six offered ones are sent"
        );
        assert_eq!(reaction_emoji(":>"), Some("😆"));
        assert_eq!(reaction_emoji("?"), None);
        for (icon, _) in DEFAULT_REACTIONS {
            assert!(reaction_emoji(icon).is_some(), "{icon}");
        }
    }
}

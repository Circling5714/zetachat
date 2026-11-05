//! Receipts.
//!
//! A conversation read on another device of the account comes as
//! `clearUnreads: [{ idTo, isGroup, lastMsgId, ts, type }]` (seen live in cmd
//! 504 frames; the key also appears, empty so far, in 610 and 611).
//!
//! This account's messages reaching the other side come as `delivereds:
//! [{ msgId, realMsgId, seen, deliveredUids, seenUids, mSTs }]` (a group's
//! items add `groupId`), and being read as `seens: [{ idTo, msgId,
//! realMsgId }]` in 1-1 chats or `groupSeens: [{ msgId, groupId, seenUids }]`
//! in groups: cmd 502 for 1-1 chats and 522 for groups, as zca-js reads them
//! (`delivereds` seen live in 502 frames).

use serde_json::Value;

/// A conversation read on another device, up to `last_msg_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadElsewhere {
    /// The other user of a 1-1 chat, or the group.
    pub thread: String,
    pub group: bool,
    pub last_msg_id: String,
}

/// This account's message delivered to, or seen by, the other side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The other user of a 1-1 chat, or the group.
    pub thread: String,
    pub group: bool,
    /// The message's ids as Zalo gives them (`msgId`, `realMsgId`).
    pub msg_ids: Vec<String>,
    pub seen: bool,
}

fn data(payload: &Value) -> &Value {
    payload
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(payload)
}

fn items<'a>(data: &'a Value, key: &str) -> &'a [Value] {
    data.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) => n.as_i64().is_some_and(|n| n != 0),
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "1" || s == "true",
        _ => false,
    }
}

/// A conversation id as the other parts of the protocol write it: no
/// `group_` or `g` prefix on a group's id.
fn conversation(id: &str, group: bool) -> String {
    let id = id.trim().trim_start_matches("group_");
    match id.strip_prefix('g') {
        Some(rest) if group && !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) => {
            rest.to_string()
        }
        _ => id.to_string(),
    }
}

fn ids(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["msgId", "realMsgId"] {
        let id = text(v, key);
        if !id.is_empty() && id != "0" && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Conversations read on another device, in a frame's payload.
pub fn read_elsewhere(payload: &Value) -> Vec<ReadElsewhere> {
    items(data(payload), "clearUnreads")
        .iter()
        .filter_map(|item| {
            let group = truthy(item.get("isGroup"));
            let thread = conversation(&text(item, "idTo"), group);
            (!thread.is_empty()).then(|| ReadElsewhere {
                thread,
                group,
                last_msg_id: text(item, "lastMsgId"),
            })
        })
        .collect()
}

/// Delivered and seen receipts of this account's messages in a frame's
/// payload; `group_frame` for cmd 522.
pub fn receipts(payload: &Value, group_frame: bool) -> Vec<Receipt> {
    let data = data(payload);
    let mut out = Vec::new();
    for item in items(data, "delivereds") {
        let group_id = text(item, "groupId");
        let group = group_frame || !group_id.is_empty();
        let thread = if group {
            conversation(&group_id, true)
        } else {
            // zca-js: a 1-1 message is delivered to the other user.
            item.get("deliveredUids")
                .and_then(Value::as_array)
                .and_then(|uids| uids.first())
                .map(|uid| match uid {
                    Value::String(s) => s.trim().to_string(),
                    other => other.to_string(),
                })
                .unwrap_or_default()
        };
        let msg_ids = ids(item);
        if !thread.is_empty() && !msg_ids.is_empty() {
            out.push(Receipt {
                thread,
                group,
                msg_ids,
                seen: truthy(item.get("seen")),
            });
        }
    }
    for (key, group) in [("seens", false), ("groupSeens", true)] {
        for item in items(data, key) {
            let thread = if group {
                conversation(&text(item, "groupId"), true)
            } else {
                text(item, "idTo")
            };
            let msg_ids = ids(item);
            if !thread.is_empty() && !msg_ids.is_empty() {
                out.push(Receipt {
                    thread,
                    group,
                    msg_ids,
                    seen: true,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conversations_read_on_another_device() {
        // The two shapes seen live in cmd 504.
        let frame = json!({ "data": { "clearUnreads": [
            { "idTo": "100", "isGroup": 0, "lastMsgId": "7001", "ts": "1700000000000", "type": 0 },
            { "idTo": "g200", "isGroup": 1, "lastCliMsgId": "5", "lastMsgId": "7002", "sct": 1,
              "ts": "1700000000001", "type": 0 }
        ], "delivereds": [], "seens": [] }, "error_code": 0 });
        assert_eq!(
            read_elsewhere(&frame),
            [
                ReadElsewhere {
                    thread: "100".into(),
                    group: false,
                    last_msg_id: "7001".into()
                },
                ReadElsewhere {
                    thread: "200".into(),
                    group: true,
                    last_msg_id: "7002".into()
                },
            ]
        );
        assert!(read_elsewhere(&json!({ "data": { "clearUnreads": [] } })).is_empty());
        assert!(read_elsewhere(&json!({})).is_empty());
    }

    #[test]
    fn delivered_and_seen() {
        // A 502 frame as seen live, with a seen receipt added.
        let frame = json!({ "data": {
            "clearUnreads": [],
            "delivereds": [{ "deliveredUids": ["300"], "mSTs": 1, "msgId": "8001",
                             "realMsgId": "0", "seen": 0, "seenUids": [] }],
            "seens": [{ "idTo": "300", "msgId": "8002", "realMsgId": "9002" }],
            "groupSeens": []
        }});
        assert_eq!(
            receipts(&frame, false),
            [
                Receipt {
                    thread: "300".into(),
                    group: false,
                    msg_ids: vec!["8001".into()],
                    seen: false
                },
                Receipt {
                    thread: "300".into(),
                    group: false,
                    msg_ids: vec!["8002".into(), "9002".into()],
                    seen: true
                },
            ]
        );
        // A 522 frame.
        let group = json!({ "data": {
            "delivereds": [{ "groupId": "400", "deliveredUids": ["1", "2"], "msgId": "8003",
                             "seen": 1, "seenUids": ["1"] }],
            "groupSeens": [{ "groupId": "400", "msgId": "8004", "seenUids": ["2"] }]
        }});
        let found = receipts(&group, true);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|r| r.group && r.thread == "400" && r.seen));
        assert!(receipts(
            &json!({ "data": { "delivereds": [{ "msgId": "1" }] } }),
            false
        )
        .is_empty());
    }
}

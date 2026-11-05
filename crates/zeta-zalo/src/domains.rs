//! Zalo service hosts.
//!
//! The defaults are hosts verified live with a QR (web/PC) session. For those
//! services the login payload (`zpw_service_map_v3`) only adds alternates, because
//! Zalo still hands out stale hosts for some of them (ctdozlite issue #22). Hosts the
//! server assigns per session (long-polling, WebSocket) are taken from the payload.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Logical Zalo services.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// `wpa.zaloapp.com`: QR login and server info.
    Auth,
    /// `wpa.chat.zalo.me`: login info of web sessions.
    WpaChat,
    /// 1-1 messages.
    Chat,
    /// Conversation list, pins, previews.
    Conversation,
    Friend,
    Profile,
    Group,
    File,
    Sticker,
    /// "Cloud của tôi" and group history.
    GroupCloudMessage,
    /// Reactions to messages.
    Reaction,
    /// Names the account gives its contacts ("tên gợi nhớ").
    Alias,
}

/// Services that `zpw_service_map_v3` can list.
const MAPPED_SERVICES: [Service; 10] = [
    Service::Chat,
    Service::Conversation,
    Service::Friend,
    Service::Profile,
    Service::Group,
    Service::File,
    Service::Sticker,
    Service::GroupCloudMessage,
    Service::Reaction,
    Service::Alias,
];

impl Service {
    /// Key of the service in `zpw_service_map_v3`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::WpaChat => "wpa_chat",
            Self::Chat => "chat",
            Self::Conversation => "conversation",
            Self::Friend => "friend",
            Self::Profile => "profile",
            Self::Group => "group",
            Self::File => "file",
            Self::Sticker => "sticker",
            Self::GroupCloudMessage => "group_cloud_message",
            Self::Reaction => "reaction",
            Self::Alias => "alias",
        }
    }
}

/// Host table of one session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Domains {
    pub auth: String,
    pub wpa_chat: String,
    pub chat: String,
    pub conversation: String,
    pub friend: String,
    pub profile: String,
    pub group: String,
    pub file: String,
    pub sticker: String,
    pub group_cloud_message: String,
    /// Sessions saved before reactions existed lack it.
    #[serde(default = "default_reaction_host")]
    pub reaction: String,
    /// Sessions saved before aliases existed lack it.
    #[serde(default = "default_alias_host")]
    pub alias: String,
    /// `zpw_chts_new[0]`: HTTP long-polling for messages.
    pub lp_chat: String,
    /// `zpw_ctrs_new[0]`: HTTP long-polling for control events.
    pub lp_control: String,
    /// `zpw_ws`: real-time WebSocket endpoints.
    pub ws: Vec<String>,
    /// Hosts from `zpw_service_map_v3`, tried after the primary host.
    #[serde(default)]
    pub alternates: BTreeMap<String, Vec<String>>,
}

impl Default for Domains {
    fn default() -> Self {
        Self {
            auth: "https://wpa.zaloapp.com".into(),
            wpa_chat: "https://wpa.chat.zalo.me".into(),
            chat: "https://tt-chat3-wpa.api.zaloapp.com".into(),
            conversation: "https://tt-convers-wpa.chat.zalo.me".into(),
            friend: "https://tt-friend-wpa.chat.zalo.me".into(),
            profile: "https://tt-profile-wpa.chat.zalo.me".into(),
            group: "https://tt-group-wpa.zaloapp.com".into(),
            file: "https://tt-files-wpa.zaloapp.com".into(),
            sticker: "https://tt-sticker-wpa.chat.zalo.me".into(),
            group_cloud_message: "https://tt-group-cm.api.zaloapp.com".into(),
            reaction: default_reaction_host(),
            alias: default_alias_host(),
            lp_chat: "https://p4-msg-chat.zaloapp.com".into(),
            lp_control: "https://p4-ctl-chat.zaloapp.com".into(),
            ws: (1..=8)
                .map(|n| format!("wss://ws{n}-msg.chat.zalo.me"))
                .collect(),
            alternates: BTreeMap::new(),
        }
    }
}

fn default_reaction_host() -> String {
    "https://reaction.api.zaloapp.com".into()
}

/// Seen live in `zpw_service_map_v3` (November 2026).
fn default_alias_host() -> String {
    "https://tt-alias-wpa.api.zaloapp.com".into()
}

/// URLs of a service-map entry, which can be a string or an array of strings.
fn urls(v: &Value) -> Vec<String> {
    let raw: Vec<&str> = match v {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        Value::String(s) => vec![s.as_str()],
        _ => Vec::new(),
    };
    raw.into_iter()
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.contains("://") {
                s.to_string()
            } else {
                format!("https://{s}")
            }
        })
        .collect()
}

impl Domains {
    /// Primary host of a service.
    pub fn primary(&self, service: Service) -> &str {
        match service {
            Service::Auth => &self.auth,
            Service::WpaChat => &self.wpa_chat,
            Service::Chat => &self.chat,
            Service::Conversation => &self.conversation,
            Service::Friend => &self.friend,
            Service::Profile => &self.profile,
            Service::Group => &self.group,
            Service::File => &self.file,
            Service::Sticker => &self.sticker,
            Service::GroupCloudMessage => &self.group_cloud_message,
            Service::Reaction => &self.reaction,
            Service::Alias => &self.alias,
        }
    }

    /// Hosts to try for a service: the primary first, then the alternates.
    ///
    /// Friend calls end with the profile host: verified live in November 2026,
    /// `getfriends` answers 404 on both `tt-friend-wpa` hosts and works on
    /// `tt-profile-wpa.chat.zalo.me` (ctdozlite has the same fallback).
    pub fn candidates(&self, service: Service) -> Vec<String> {
        let mut out = vec![self.primary(service).to_string()];
        let extra = self.alternates.get(service.key()).into_iter().flatten();
        let profile_fallback = (service == Service::Friend).then_some(&self.profile);
        for host in extra.chain(profile_fallback) {
            if !out.contains(host) {
                out.push(host.clone());
            }
        }
        out
    }

    /// Absorb the endpoint data of `getServerInfo` / `getLoginInfo`.
    pub fn apply_login_config(&mut self, cfg: &Value) {
        if let Some(map) = cfg.get("zpw_service_map_v3") {
            for service in MAPPED_SERVICES {
                let primary = self.primary(service).to_string();
                let extra: Vec<String> = map
                    .get(service.key())
                    .map(urls)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|u| *u != primary)
                    .collect();
                if !extra.is_empty() {
                    self.alternates.insert(service.key().to_string(), extra);
                }
            }
        }
        if let Some(v) = cfg
            .get("zpw_chts_new")
            .map(urls)
            .and_then(|u| u.into_iter().next())
        {
            self.lp_chat = v;
        }
        if let Some(v) = cfg
            .get("zpw_ctrs_new")
            .map(urls)
            .and_then(|u| u.into_iter().next())
        {
            self.lp_control = v;
        }
        let ws: Vec<String> = cfg
            .get("zpw_ws")
            .map(urls)
            .unwrap_or_default()
            .into_iter()
            .filter(|u| u.starts_with("wss://") || u.starts_with("ws://"))
            .collect();
        if !ws.is_empty() {
            self.ws = ws;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_the_verified_hosts() {
        let d = Domains::default();
        assert_eq!(
            d.primary(Service::Profile),
            "https://tt-profile-wpa.chat.zalo.me"
        );
        assert_eq!(
            d.primary(Service::Friend),
            "https://tt-friend-wpa.chat.zalo.me"
        );
        assert_eq!(d.ws.len(), 8);
    }

    #[test]
    fn friend_calls_fall_back_to_the_profile_host() {
        let d = Domains::default();
        assert_eq!(
            d.candidates(Service::Friend),
            vec![
                "https://tt-friend-wpa.chat.zalo.me".to_string(),
                "https://tt-profile-wpa.chat.zalo.me".to_string(),
            ]
        );
        assert_eq!(d.candidates(Service::Profile).len(), 1);
    }

    #[test]
    fn login_config_adds_alternates_but_keeps_verified_primaries() {
        let mut d = Domains::default();
        d.apply_login_config(&json!({
            "zpw_service_map_v3": {
                "profile": ["https://tt-profile-wpa.zaloapp.com", "https://tt-profile-wpa.chat.zalo.me/"],
                "friend": "https://tt-friend-wpa.zaloapp.com"
            },
            "zpw_chts_new": ["p5-msg-chat.zaloapp.com"],
            "zpw_ctrs_new": "https://p5-ctl-chat.zaloapp.com/",
            "zpw_ws": ["wss://ws3-msg.chat.zalo.me", "not-a-socket"]
        }));
        assert_eq!(d.profile, "https://tt-profile-wpa.chat.zalo.me");
        assert_eq!(
            d.candidates(Service::Profile),
            vec![
                "https://tt-profile-wpa.chat.zalo.me".to_string(),
                "https://tt-profile-wpa.zaloapp.com".to_string(),
            ]
        );
        // Primary, the service-map alternate, then the profile fallback.
        assert_eq!(d.candidates(Service::Friend).len(), 3);
        assert_eq!(d.lp_chat, "https://p5-msg-chat.zaloapp.com");
        assert_eq!(d.lp_control, "https://p5-ctl-chat.zaloapp.com");
        assert_eq!(d.ws, vec!["wss://ws3-msg.chat.zalo.me".to_string()]);
    }

    #[test]
    fn empty_config_changes_nothing() {
        let mut d = Domains::default();
        d.apply_login_config(&json!({}));
        assert_eq!(d, Domains::default());
    }

    #[test]
    fn serde_tolerates_missing_alternates() {
        let mut v = serde_json::to_value(Domains::default()).unwrap();
        v.as_object_mut().unwrap().remove("alternates");
        let d: Domains = serde_json::from_value(v).unwrap();
        assert!(d.alternates.is_empty());
    }
}

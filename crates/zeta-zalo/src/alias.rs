//! The names the account gives its contacts ("tên gợi nhớ"), kept by Zalo and
//! shown on all the account's devices instead of the contact's own name. As
//! zca-js does it, on the `alias` service:
//!
//! - List: `GET /api/alias/list` with `{ page, count, imei }` answers
//!   `{ items: [{ userId, alias }], updateTime }`.
//! - Set: `GET /api/alias/update` with `{ friendId, alias, imei }`.
//! - Remove: `GET /api/alias/remove` with `{ friendId }`.

use serde_json::{json, Value};

use crate::api::first_str;
use crate::client::ZaloClient;
use crate::domains::Service;
use crate::error::Result;

/// Aliases asked per page.
const PAGE: usize = 100;
/// Pages read at most (10 000 aliases).
const MAX_PAGES: usize = 100;

/// `(user id, alias)` of a list page.
fn aliases_of(data: &Value) -> Vec<(String, String)> {
    let items = data
        .get("items")
        .or_else(|| data.get("data").and_then(|d| d.get("items")))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            let uid = first_str(item, &["userId", "uid"]);
            let alias = first_str(item, &["alias"]).trim().to_string();
            (!uid.is_empty() && !alias.is_empty()).then_some((uid, alias))
        })
        .collect()
}

impl ZaloClient {
    /// Every alias the account gave: `(user id, alias)`.
    pub async fn aliases(&self) -> Result<Vec<(String, String)>> {
        let key = self.session_key()?;
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let params = self.encrypt_params(&json!({
                "page": page,
                "count": PAGE,
                "imei": self.device().imei,
            }))?;
            let path = format!("/api/alias/list?{}&params={params}", Self::common_params());
            let data = self.get_service(Service::Alias, &path, Some(key)).await?;
            let found = aliases_of(&data);
            let full = found.len() >= PAGE;
            all.extend(found);
            if !full {
                break;
            }
        }
        Ok(all)
    }

    /// Give `uid` an alias; an empty one removes it.
    pub async fn set_alias(&self, uid: &str, alias: &str) -> Result<()> {
        let key = self.session_key()?;
        let alias = alias.trim();
        let (path, params) = if alias.is_empty() {
            ("/api/alias/remove", json!({ "friendId": uid }))
        } else {
            (
                "/api/alias/update",
                json!({ "friendId": uid, "alias": alias, "imei": self.device().imei }),
            )
        };
        let path = format!(
            "{path}?{}&params={}",
            Self::common_params(),
            self.encrypt_params(&params)?
        );
        // The answer is empty on success; errors come as error codes.
        match self.get_service(Service::Alias, &path, Some(key)).await {
            Ok(_) | Err(crate::Error::EmptyResponse { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_pages() {
        let page = json!({ "items": [
            { "userId": "1", "alias": " Mẹ yêu " },
            { "userId": "2", "alias": "" },
            { "alias": "không id" }
        ], "updateTime": "1700000000" });
        assert_eq!(aliases_of(&page), [("1".to_string(), "Mẹ yêu".to_string())]);
        assert_eq!(aliases_of(&json!({ "data": page })).len(), 1);
        assert!(aliases_of(&json!({})).is_empty());
    }
}

//! The groups on the server.

use serde_json::Value;

use crate::client::{Client, HttpError};
use crate::syncplay::update::text;

/// One group in the server's list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupSummary {
    /// Group id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Distinct participants.
    pub members: usize,
    /// Server state name.
    pub state: String,
}

/// Parses the `GET /SyncPlay/List` body, skipping malformed entries.
#[must_use]
pub fn parse_groups(body: &Value) -> Vec<GroupSummary> {
    let Some(list) = body.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|g| {
            Some(GroupSummary {
                id: text(g, "GroupId")?.to_owned(),
                name: text(g, "GroupName")?.to_owned(),
                members: g.get("Participants")?.as_array()?.len(),
                state: text(g, "State").unwrap_or("").to_owned(),
            })
        })
        .collect()
}

impl Client {
    /// The server's groups.
    ///
    /// # Errors
    /// The request failed.
    pub fn list_groups(&self) -> Result<Vec<GroupSummary>, HttpError> {
        Ok(parse_groups(&self.get("/SyncPlay/List")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_groups_tolerates_bad_entries() {
        let body = json!([
            {"GroupId": "g1", "GroupName": "One", "State": "Idle", "Participants": ["a", "b"]},
            {"GroupName": "no id", "Participants": []},
            "junk",
            {"GroupId": "g3", "GroupName": "Three", "Participants": []}
        ]);
        assert_eq!(
            parse_groups(&body),
            vec![
                GroupSummary {
                    id: "g1".into(),
                    name: "One".into(),
                    members: 2,
                    state: "Idle".into()
                },
                GroupSummary {
                    id: "g3".into(),
                    name: "Three".into(),
                    members: 0,
                    state: String::new()
                },
            ]
        );
        assert!(parse_groups(&json!({"a": 1})).is_empty());
    }
}

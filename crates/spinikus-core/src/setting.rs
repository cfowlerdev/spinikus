//! Instance-wide settings, managed through the API: one JSON value per key.

use jiff::Timestamp;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Setting {
    pub key: String,
    pub value: serde_json::Value,
    /// 0 for a setting that was never set, which has its default value.
    pub revision: i64,
    /// `None` for a setting that was never set.
    pub updated_at: Option<Timestamp>,
}

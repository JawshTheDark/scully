// Lenient deserialization helpers.
//
// Two real bugs share one anatomy: `alt` typed as a string (boolean on the
// wire) and `speakers` typed as `Vec<String>` (objects on the wire). serde
// fails an entire frame on one bad field, so each wrong guess silently threw
// away whole `history`/`backlog` frames — and both passed local testing, where
// the field happened to be empty. These helpers bound the cost of the next
// wrong guess: a decorative field falls back to its default, and an events
// array is salvaged row-by-row.

use serde::{Deserialize, Deserializer};

use crate::event::MessageEvent;

/// Parse a non-essential field, falling back to `T::default()` on mismatch.
pub(crate) fn lenient<'de, T, D>(d: D) -> Result<T, D::Error>
where
    T: Default + serde::de::DeserializeOwned,
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "lenient field failed to parse; using default");
        T::default()
    }))
}

/// Salvage an events array row-by-row: the rows are the payload, so a bad row
/// is dropped and logged instead of taking the rest of the frame with it.
pub(crate) fn lenient_events<'de, D>(d: D) -> Result<Vec<MessageEvent>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = Vec::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| match serde_json::from_value::<MessageEvent>(value.clone()) {
            Ok(event) => Some(event),
            Err(e) => {
                tracing::warn!(error = %e, row = %value, "dropping one undecodable event row");
                None
            }
        })
        .collect())
}

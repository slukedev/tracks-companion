//! Tolerant decoding helpers. Music Assistant adds fields and enum values between releases and
//! sends `null` for many values, so one odd field must never fail a whole player, queue or list.

use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer};
use serde_json::Value;

/// A missing or `null` value decodes as the type's default.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A missing or `null` value decodes as `true`.
pub(crate) fn nullable_true<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(true))
}

pub(crate) const fn yes() -> bool {
    true
}

/// A value that does not decode as `T` becomes `None` instead of an error.
pub(crate) fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    Ok(serde_json::from_value(value).ok())
}

/// [`lenient`] for boxed values.
pub(crate) fn lenient_box<'de, D, T>(deserializer: D) -> Result<Option<Box<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(lenient::<D, T>(deserializer)?.map(Box::new))
}

/// A list whose entries that do not decode as `T` are dropped. `null` or a non-list is empty.
pub(crate) fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(items) => decode_entries(items).0,
        _ => Vec::new(),
    })
}

/// Decodes each entry on its own, returning the decoded entries and how many were dropped.
pub(crate) fn decode_entries<T: DeserializeOwned>(items: Vec<Value>) -> (Vec<T>, usize) {
    let total = items.len();
    let decoded: Vec<T> = items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect();
    let dropped = total - decoded.len();
    (decoded, dropped)
}

/// A whole number that may arrive as an integer, a float or a numeric string. Anything else
/// (including negative numbers) is `None`.
pub(crate) fn lenient_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(number_of(&Value::deserialize(deserializer)?)
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= f64::from(u32::MAX))
        .map(|n| n as u32))
}

/// A number that may arrive as an integer, a float or a numeric string.
pub(crate) fn lenient_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(number_of(&Value::deserialize(deserializer)?).filter(|n| n.is_finite()))
}

/// [`lenient_f64`] with `0.0` for anything that is not a number.
pub(crate) fn lenient_f64_or_zero<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(lenient_f64(deserializer)?.unwrap_or(0.0))
}

/// [`lenient_u32`] with `0` for anything that is not a whole number.
pub(crate) fn lenient_u32_or_zero<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(lenient_u32(deserializer)?.unwrap_or(0))
}

fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A list of strings, dropping entries that are not strings.
pub(crate) fn string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct Sample {
        #[serde(default, deserialize_with = "super::lenient_u32")]
        count: Option<u32>,
        #[serde(default, deserialize_with = "super::lenient_vec")]
        items: Vec<u8>,
        #[serde(default, deserialize_with = "super::nullable")]
        name: String,
        #[serde(default, deserialize_with = "super::string_list")]
        tags: Vec<String>,
    }

    fn sample(value: serde_json::Value) -> Sample {
        serde_json::from_value(value).expect("decodes")
    }

    #[test]
    fn whole_numbers_accept_floats_and_strings() {
        assert_eq!(sample(json!({"count": 12.0})).count, Some(12));
        assert_eq!(sample(json!({"count": "7"})).count, Some(7));
    }

    #[test]
    fn whole_numbers_reject_negatives_and_garbage() {
        assert_eq!(sample(json!({"count": -1})).count, None);
        assert_eq!(sample(json!({"count": {"a": 1}})).count, None);
    }

    #[test]
    fn lists_drop_entries_that_do_not_decode() {
        assert_eq!(sample(json!({"items": [1, "x", 300, 2]})).items, vec![1, 2]);
    }

    #[test]
    fn null_strings_become_empty() {
        assert_eq!(sample(json!({"name": null})).name, "");
    }

    #[test]
    fn string_lists_keep_only_strings() {
        assert_eq!(
            sample(json!({"tags": ["a", 1, null, "b"]})).tags,
            vec!["a", "b"]
        );
    }
}

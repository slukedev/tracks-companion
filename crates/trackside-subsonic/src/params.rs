//! Request parameters from the query string and, for formPost, the body.

use percent_encoding::percent_decode_str;

use crate::error::ApiError;

/// Every parameter of a request in arrival order: the query string first, then a
/// `application/x-www-form-urlencoded` body. Repeated names (`id=a&id=b`) are all kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params(Vec<(String, String)>);

/// Decodes one `application/x-www-form-urlencoded` component: `+` is a space and `%2B` a plus.
fn decode_component(raw: &str) -> String {
    let spaced = raw.replace('+', " ");
    percent_decode_str(&spaced).decode_utf8_lossy().into_owned()
}

impl Params {
    /// Parses a query string or form body and appends its pairs.
    pub fn extend_from_encoded(&mut self, encoded: &str) {
        for pair in encoded.split('&').filter(|p| !p.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            self.0
                .push((decode_component(name), decode_component(value)));
        }
    }

    pub fn from_encoded(encoded: &str) -> Self {
        let mut params = Self::default();
        params.extend_from_encoded(encoded);
        params
    }

    /// The first value for `name`. Empty values count as absent, as clients send `size=` for
    /// "default".
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, v)| n == name && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }

    /// The first value for `name`, even when empty.
    pub fn raw(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every non-empty value for `name`, in order.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(n, v)| n == name && !v.is_empty())
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn required(&self, name: &str) -> Result<&str, ApiError> {
        self.get(name).ok_or_else(|| ApiError::missing(name))
    }

    pub fn bool(&self, name: &str) -> Result<Option<bool>, ApiError> {
        self.get(name)
            .map(|v| match v.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(true),
                "false" | "0" | "no" => Ok(false),
                _ => Err(ApiError::invalid(name)),
            })
            .transpose()
    }

    pub fn int<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, ApiError> {
        self.get(name)
            .map(|v| v.trim().parse().map_err(|_| ApiError::invalid(name)))
            .transpose()
    }

    /// Every value for `name` parsed as `T`.
    pub fn ints<T: std::str::FromStr>(&self, name: &str) -> Result<Vec<T>, ApiError> {
        self.all(name)
            .into_iter()
            .map(|v| v.trim().parse().map_err(|_| ApiError::invalid(name)))
            .collect()
    }

    /// A count with a default, capped at `max`.
    pub fn count(&self, name: &str, default: u32, max: u32) -> Result<u32, ApiError> {
        Ok(self.int::<u32>(name)?.unwrap_or(default).min(max))
    }

    pub fn offset(&self, name: &str) -> Result<usize, ApiError> {
        Ok(self.int::<usize>(name)?.unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_sign_in_query_is_decoded() {
        let params =
            Params::from_encoded("u=luke%2Bmusic%40example.com&query=Florence+%2B+the+Machine");
        assert_eq!(params.get("u"), Some("luke+music@example.com"));
        assert_eq!(params.get("query"), Some("Florence + the Machine"));
    }

    #[test]
    fn repeated_names_keep_every_value_in_order() {
        let params = Params::from_encoded("id=s1&albumId=a1&id=s2&id=");
        assert_eq!(params.all("id"), vec!["s1", "s2"]);
        assert_eq!(params.get("albumId"), Some("a1"));
    }

    #[test]
    fn empty_values_count_as_absent_but_stay_readable() {
        let params = Params::from_encoded("query=&size=");
        assert_eq!(params.get("query"), None);
        assert_eq!(params.raw("query"), Some(""));
        assert_eq!(params.count("size", 10, 500).ok(), Some(10));
    }

    #[test]
    fn form_body_adds_to_the_query() {
        let mut params = Params::from_encoded("f=json");
        params.extend_from_encoded("u=luke&p=enc%3A70617373");
        assert_eq!(params.get("f"), Some("json"));
        assert_eq!(params.get("p"), Some("enc:70617373"));
    }

    #[test]
    fn numbers_and_flags_parse_or_fail_with_code_0() {
        let params = Params::from_encoded("size=900&submission=false&offset=x&flag=maybe");
        assert_eq!(params.count("size", 10, 500).ok(), Some(500));
        assert_eq!(params.bool("submission").ok(), Some(Some(false)));
        assert_eq!(params.offset("offset").map_err(|e| e.code), Err(0));
        assert_eq!(params.bool("flag").map_err(|e| e.code), Err(0));
        assert_eq!(params.required("id").map_err(|e| e.code), Err(10));
    }

    #[test]
    fn a_pair_without_equals_is_a_name_with_no_value() {
        let params = Params::from_encoded("estimateContentLength&id=s1");
        assert_eq!(params.raw("estimateContentLength"), Some(""));
        assert_eq!(params.get("id"), Some("s1"));
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::Params;

    /// What Tracks' `SubsonicClient` sends: `URLComponents` encoding, then `+` as `%2B`.
    fn encode(value: &str) -> String {
        value
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    proptest! {
        #[test]
        fn values_survive_the_encoding_clients_use(
            pairs in proptest::collection::vec(("[a-zA-Z]{1,12}", any::<String>()), 0..8),
        ) {
            let query: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{k}={}", encode(v)))
                .collect();
            let params = Params::from_encoded(&query.join("&"));
            for (k, v) in &pairs {
                let all = params.all(k);
                if !v.is_empty() {
                    prop_assert!(all.contains(&v.as_str()), "{k}={v:?} became {all:?}");
                }
            }
        }

        #[test]
        fn arbitrary_query_strings_never_panic(raw in any::<String>(), name in "[a-z]{1,6}") {
            let params = Params::from_encoded(&raw);
            let _ = params.get(&name);
            let _ = params.bool(&name);
            let _ = params.int::<i64>(&name);
            let _ = params.ints::<u32>(&name);
            let _ = params.all(&name);
        }

        #[test]
        fn a_plus_is_a_space_and_an_encoded_plus_is_a_plus(left in "[a-z]{1,8}", right in "[a-z]{1,8}") {
            let params = Params::from_encoded(&format!("q={left}+{right}&r={left}%2B{right}"));
            let spaced = format!("{left} {right}");
            let plus = format!("{left}+{right}");
            prop_assert_eq!(params.get("q"), Some(spaced.as_str()));
            prop_assert_eq!(params.get("r"), Some(plus.as_str()));
        }
    }
}

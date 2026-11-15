//! One response model, written as Subsonic XML or JSON.
//!
//! Subsonic's JSON is its XML read through a fixed convention: attributes become fields, child
//! elements become objects or arrays, and element text becomes `value` (or a bare value for
//! text-only elements such as `biography`). A [`Node`] records which children are lists so
//! that JSON always sends an array for them, even with one item, and never for single
//! elements.

use std::fmt::Write as _;

use serde_json::{Map, Number, Value as Json};

use crate::error::ApiError;

pub const API_VERSION: &str = "1.16.1";
pub const SERVER_TYPE: &str = "trackside";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const XMLNS: &str = "http://subsonic.org/restapi";

/// A scalar: an attribute value or the text of a text-only element.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Value {
    fn to_json(&self) -> Json {
        match self {
            Self::Str(s) => Json::String(s.clone()),
            Self::Int(i) => Json::Number((*i).into()),
            Self::Float(f) => Number::from_f64(*f).map_or(Json::Null, Json::Number),
            Self::Bool(b) => Json::Bool(*b),
        }
    }

    fn to_text(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Self::Str(v.clone())
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}
macro_rules! int_value {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Self::Int(i64::from(v))
            }
        }
    )*};
}
int_value!(i32, u32, i64, u8, u16);
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<usize> for Value {
    fn from(v: usize) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

/// A child slot of a [`Node`].
#[derive(Clone, Debug, PartialEq)]
pub enum Child {
    One(Node),
    Many(Vec<Node>),
    /// A text-only element: `<biography>..</biography>`, JSON `"biography": ".."`.
    Text(Value),
    /// Repeated text-only elements: `<isrc>..</isrc>`, JSON `"isrc": [..]`.
    Texts(Vec<Value>),
}

/// An element: attributes, children in order, and optional text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    attrs: Vec<(&'static str, Value)>,
    children: Vec<(&'static str, Child)>,
    text: Option<String>,
}

impl Node {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn attr(mut self, name: &'static str, value: impl Into<Value>) -> Self {
        self.set(name, value);
        self
    }

    #[must_use]
    pub fn attr_opt<V: Into<Value>>(mut self, name: &'static str, value: Option<V>) -> Self {
        if let Some(value) = value {
            self.set(name, value);
        }
        self
    }

    pub fn set(&mut self, name: &'static str, value: impl Into<Value>) {
        self.attrs.push((name, value.into()));
    }

    #[must_use]
    pub fn one(mut self, name: &'static str, node: Node) -> Self {
        self.children.push((name, Child::One(node)));
        self
    }

    #[must_use]
    pub fn many(mut self, name: &'static str, nodes: Vec<Node>) -> Self {
        self.children.push((name, Child::Many(nodes)));
        self
    }

    #[must_use]
    pub fn text_child(mut self, name: &'static str, value: impl Into<Value>) -> Self {
        self.children.push((name, Child::Text(value.into())));
        self
    }

    #[must_use]
    pub fn texts(mut self, name: &'static str, values: Vec<Value>) -> Self {
        self.children.push((name, Child::Texts(values)));
        self
    }

    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// The value of an attribute, for tests and for building related nodes.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    fn to_json(&self) -> Json {
        let mut map = Map::new();
        for (name, value) in &self.attrs {
            map.insert((*name).to_owned(), value.to_json());
        }
        for (name, child) in &self.children {
            let json = match child {
                Child::One(node) => node.to_json(),
                Child::Many(nodes) => Json::Array(nodes.iter().map(Self::to_json).collect()),
                Child::Text(value) => value.to_json(),
                Child::Texts(values) => Json::Array(values.iter().map(Value::to_json).collect()),
            };
            map.insert((*name).to_owned(), json);
        }
        if let Some(text) = &self.text {
            map.insert(String::from("value"), Json::String(text.clone()));
        }
        Json::Object(map)
    }

    fn write_xml(&self, name: &str, extra: &[(&str, String)], out: &mut String) {
        out.push('<');
        out.push_str(name);
        for (key, value) in extra {
            write_attr(out, key, value);
        }
        for (key, value) in &self.attrs {
            write_attr(out, key, &value.to_text());
        }
        let has_children = self.children.iter().any(|(_, child)| match child {
            Child::Many(nodes) => !nodes.is_empty(),
            Child::Texts(values) => !values.is_empty(),
            Child::One(_) | Child::Text(_) => true,
        });
        if !has_children && self.text.is_none() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        for (child_name, child) in &self.children {
            match child {
                Child::One(node) => node.write_xml(child_name, &[], out),
                Child::Many(nodes) => {
                    for node in nodes {
                        node.write_xml(child_name, &[], out);
                    }
                }
                Child::Text(value) => write_text_element(out, child_name, value),
                Child::Texts(values) => {
                    for value in values {
                        write_text_element(out, child_name, value);
                    }
                }
            }
        }
        if let Some(text) = &self.text {
            out.push_str(&escape(text));
        }
        let _ = write!(out, "</{name}>");
    }
}

/// Characters XML 1.0 cannot carry at all, even escaped. Tags occasionally contain them.
fn xml_allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{FFFE}' && c != '\u{FFFF}')
}

fn escape(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| xml_allowed(*c)).collect();
    quick_xml::escape::escape(cleaned.as_str()).into_owned()
}

fn write_attr(out: &mut String, key: &str, value: &str) {
    let _ = write!(out, " {key}=\"{}\"", escape(value));
}

fn write_text_element(out: &mut String, name: &str, value: &Value) {
    let _ = write!(out, "<{name}>{}</{name}>", escape(&value.to_text()));
}

/// The requested response format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Xml,
    Json,
    /// Asked for, not supported: answered with JSON error 0.
    Jsonp,
}

impl Format {
    pub fn parse(f: Option<&str>) -> Self {
        match f.map(str::to_ascii_lowercase).as_deref() {
            Some("json") => Self::Json,
            Some("jsonp") => Self::Jsonp,
            _ => Self::Xml,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Xml => "text/xml; charset=utf-8",
            Self::Json | Self::Jsonp => "application/json; charset=utf-8",
        }
    }
}

/// What goes inside `subsonic-response`.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Empty,
    Payload(&'static str, Child),
    Error(ApiError),
}

impl Body {
    pub fn one(name: &'static str, node: Node) -> Self {
        Self::Payload(name, Child::One(node))
    }

    pub fn many(name: &'static str, nodes: Vec<Node>) -> Self {
        Self::Payload(name, Child::Many(nodes))
    }
}

/// Writes the whole envelope.
pub fn render(body: &Body, format: Format) -> String {
    let status = if matches!(body, Body::Error(_)) {
        "failed"
    } else {
        "ok"
    };
    let mut root = Node::new()
        .attr("status", status)
        .attr("version", API_VERSION)
        .attr("type", SERVER_TYPE)
        .attr("serverVersion", SERVER_VERSION)
        .attr("openSubsonic", true);
    match body {
        Body::Empty => {}
        Body::Payload(name, child) => root.children.push((name, child.clone())),
        Body::Error(error) => {
            let node = Node::new()
                .attr("code", error.code)
                .attr("message", error.message.as_str())
                .attr_opt("helpUrl", error.help_url.as_deref());
            root = root.one("error", node);
        }
    }
    match format {
        Format::Xml => {
            let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            root.write_xml(
                "subsonic-response",
                &[("xmlns", String::from(XMLNS))],
                &mut out,
            );
            out
        }
        Format::Json | Format::Jsonp => {
            let mut envelope = Map::new();
            envelope.insert(String::from("subsonic-response"), root.to_json());
            Json::Object(envelope).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Body {
        let song = Node::new()
            .attr("id", "s1")
            .attr("title", "Rock & <Roll> \"Live\"\u{0001}")
            .attr("duration", 261)
            .attr("isDir", false)
            .attr_opt::<u32>("year", None)
            .texts("isrc", vec![Value::from("USSM10804555")])
            .many("genres", vec![Node::new().attr("name", "Pop")])
            .one("replayGain", Node::new().attr("trackGain", -8.21));
        Body::one("song", song)
    }

    #[test]
    fn ping_envelope_in_json() {
        insta::assert_snapshot!(render(&Body::Empty, Format::Json), @r#"{"subsonic-response":{"openSubsonic":true,"serverVersion":"0.1.0","status":"ok","type":"trackside","version":"1.16.1"}}"#);
    }

    #[test]
    fn ping_envelope_in_xml() {
        insta::assert_snapshot!(render(&Body::Empty, Format::Xml), @r#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="trackside" serverVersion="0.1.0" openSubsonic="true"/>"#);
    }

    #[test]
    fn errors_carry_code_message_and_help_url() {
        let error = ApiError::new(41, "Token authentication not supported")
            .with_help_url(Some(String::from("https://music.example.com/#/account")));
        insta::assert_snapshot!(render(&Body::Error(error.clone()), Format::Json), @r#"{"subsonic-response":{"error":{"code":41,"helpUrl":"https://music.example.com/#/account","message":"Token authentication not supported"},"openSubsonic":true,"serverVersion":"0.1.0","status":"failed","type":"trackside","version":"1.16.1"}}"#);
        insta::assert_snapshot!(render(&Body::Error(error), Format::Xml), @r#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1" type="trackside" serverVersion="0.1.0" openSubsonic="true"><error code="41" message="Token authentication not supported" helpUrl="https://music.example.com/#/account"/></subsonic-response>"#);
    }

    #[test]
    fn lists_are_arrays_and_text_is_escaped() {
        insta::assert_snapshot!(render(&sample(), Format::Json), @r#"{"subsonic-response":{"openSubsonic":true,"serverVersion":"0.1.0","song":{"duration":261,"genres":[{"name":"Pop"}],"id":"s1","isDir":false,"isrc":["USSM10804555"],"replayGain":{"trackGain":-8.21},"title":"Rock & <Roll> \"Live\"\u0001"},"status":"ok","type":"trackside","version":"1.16.1"}}"#);
        insta::assert_snapshot!(render(&sample(), Format::Xml), @r#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="trackside" serverVersion="0.1.0" openSubsonic="true"><song id="s1" title="Rock &amp; &lt;Roll&gt; &quot;Live&quot;" duration="261" isDir="false"><isrc>USSM10804555</isrc><genres name="Pop"/><replayGain trackGain="-8.21"/></song></subsonic-response>"#);
    }

    #[test]
    fn xml_output_parses() {
        let xml = render(&sample(), Format::Xml);
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut elements = 0;
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(quick_xml::events::Event::Start(_) | quick_xml::events::Event::Empty(_)) => {
                    elements += 1;
                }
                Ok(_) => {}
                Err(error) => panic!("invalid XML: {error}"),
            }
        }
        assert_eq!(elements, 5);
    }

    #[test]
    fn empty_lists_are_empty_arrays_in_json_and_nothing_in_xml() {
        let body = Body::one("albumList2", Node::new().many("album", Vec::new()));
        assert!(render(&body, Format::Json).contains(r#""albumList2":{"album":[]}"#));
        assert!(render(&body, Format::Xml).contains("<albumList2/>"));
    }

    #[test]
    fn text_nodes_become_value_in_json() {
        let body = Body::one(
            "lyrics",
            Node::new().attr("artist", "A").text("line one\nline two"),
        );
        assert!(
            render(&body, Format::Json)
                .contains(r#""lyrics":{"artist":"A","value":"line one\nline two"}"#)
        );
        assert!(
            render(&body, Format::Xml).contains("<lyrics artist=\"A\">line one\nline two</lyrics>")
        );
    }

    #[test]
    fn format_defaults_to_xml() {
        assert_eq!(Format::parse(None), Format::Xml);
        assert_eq!(Format::parse(Some("JSON")), Format::Json);
        assert_eq!(Format::parse(Some("jsonp")), Format::Jsonp);
        assert_eq!(Format::parse(Some("yaml")), Format::Xml);
    }
}

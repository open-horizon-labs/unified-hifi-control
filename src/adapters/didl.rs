//! DIDL-Lite metadata parsing, shared by the OpenHome and UPnP adapters.
//!
//! Both backends carry track metadata as DIDL-Lite: OpenHome via its own
//! metadata event, UPnP via `AVTransport::GetPositionInfo`'s `TrackMetaData`.
//! In both cases the payload arrives XML-escaped inside another XML document,
//! so it must be [`html_decode`]d before [`parse_didl_lite`] can see the tags.
//!
//! This module exists so the two adapters cannot drift apart in how they read
//! the same format.

use serde::{Deserialize, Serialize};

/// Track metadata extracted from a DIDL-Lite document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_art_uri: Option<String>,
    pub genre: Option<String>,
}

/// Decode the XML entities that wrap escaped DIDL-Lite payloads.
///
/// `&amp;` is replaced **last**, so a double-encoded entity decodes exactly one
/// level: `&amp;lt;` becomes `&lt;`, not `<`. Replacing it earlier would let the
/// `&` it produces be re-consumed by a later replacement.
pub fn html_decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Read the text content of `tag`, tolerating attributes on the opening tag.
///
/// Two tolerances, each load-bearing rather than cosmetic:
///
/// - **Attributes.** Real renderers emit
///   `<upnp:albumArtURI dlna:profileID="JPEG_TN">`, and a matcher accepting only
///   a bare `<tag>` silently returns nothing for the most common form of the one
///   field album art depends on.
/// - **Namespace prefix, when `tag` carries none.** SOAP responses may or may
///   not namespace their elements, so `CurrentVolume` also matches
///   `<u:CurrentVolume>`. When `tag` *does* carry a prefix (`upnp:album`) the
///   match is exact, which is what keeps `upnp:album` from swallowing
///   `upnp:albumArtURI` and what DIDL-Lite callers rely on.
pub fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
    // A prefixed tag is matched verbatim; an unprefixed one may appear with any
    // namespace prefix.
    if tag.contains(':') {
        return extract_exact(xml, tag);
    }
    extract_exact(xml, tag).or_else(|| extract_prefixed(xml, tag))
}

/// Match `<tag ...>value</tag>` with the tag name exactly as given.
fn extract_exact(xml: &str, tag: &str) -> Option<String> {
    let end_tag = format!("</{}>", tag);
    let mut from = 0;

    while let Some(rel) = xml[from..].find(&format!("<{}", tag)) {
        let open_start = from + rel;
        let after_name = open_start + 1 + tag.len();
        let rest = xml.get(after_name..)?;

        // The character after the tag name decides whether this is our tag.
        match rest.chars().next() {
            // Exact match, no attributes.
            Some('>') => {
                let value_start = after_name + 1;
                let end = xml[value_start..].find(&end_tag)? + value_start;
                return Some(xml[value_start..end].to_string());
            }
            // Attributes present: skip past them to the end of the opening tag.
            Some(c) if c.is_whitespace() => {
                let close_rel = rest.find('>')?;
                // A self-closing tag carries no text content.
                if rest[..close_rel].ends_with('/') {
                    from = after_name + close_rel + 1;
                    continue;
                }
                let value_start = after_name + close_rel + 1;
                let end = xml[value_start..].find(&end_tag)? + value_start;
                return Some(xml[value_start..end].to_string());
            }
            // A longer tag name that merely starts with ours — keep looking.
            _ => {
                from = after_name;
            }
        }
    }

    None
}

/// Match the same tag carrying any single namespace prefix, e.g. `<u:tag>`.
fn extract_prefixed(xml: &str, tag: &str) -> Option<String> {
    let needle = format!(":{}", tag);
    let mut from = 0;

    while let Some(rel) = xml[from..].find(&needle) {
        let colon = from + rel;
        // Walk back over the prefix to the '<' that opens this tag.
        let open = xml[..colon].rfind('<')?;
        let prefix = &xml[open + 1..colon];
        // A prefix is a single name: no '>', no whitespace, no nested '<'.
        if prefix.is_empty() || prefix.contains(['>', '<', ' ', '\t', '\n', '/']) {
            from = colon + needle.len();
            continue;
        }
        let qualified = format!("{}:{}", prefix, tag);
        if let Some(v) = extract_exact(&xml[open..], &qualified) {
            return Some(v);
        }
        from = colon + needle.len();
    }

    None
}

/// Parse a decoded DIDL-Lite document into [`TrackInfo`].
///
/// Always returns `Some`: absent fields become empty strings or `None`, which
/// is what both adapters have always relied on.
pub fn parse_didl_lite(xml: &str) -> Option<TrackInfo> {
    let title = extract_xml_value(xml, "dc:title")
        .or_else(|| extract_xml_value(xml, "title"))
        .unwrap_or_default();

    let artist = extract_xml_value(xml, "upnp:artist")
        .or_else(|| extract_xml_value(xml, "dc:creator"))
        .unwrap_or_default();

    let album = extract_xml_value(xml, "upnp:album").unwrap_or_default();
    let album_art_uri = extract_xml_value(xml, "upnp:albumArtURI");
    let genre = extract_xml_value(xml, "upnp:genre");

    Some(TrackInfo {
        title,
        artist,
        album,
        album_art_uri,
        genre,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic DIDL-Lite payload as a renderer returns it, with attributes
    /// on the tags that carry them in practice.
    const REAL_DIDL: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1" parentID="0" restricted="1"><dc:title>Hoppipolla</dc:title><upnp:artist>Sigur Ros</upnp:artist><upnp:album>Takk...</upnp:album><upnp:genre>Post-rock</upnp:genre><upnp:albumArtURI dlna:profileID="JPEG_TN">http://10.0.0.5/art/42.jpg</upnp:albumArtURI></item></DIDL-Lite>"#;

    /// The same payload with a bare albumArtURI tag (no attributes).
    const BARE_DIDL: &str = r#"<DIDL-Lite><item><dc:title>Hoppipolla</dc:title><upnp:artist>Sigur Ros</upnp:artist><upnp:album>Takk...</upnp:album><upnp:albumArtURI>http://10.0.0.5/art/42.jpg</upnp:albumArtURI></item></DIDL-Lite>"#;

    #[test]
    fn html_decode_unescapes_in_the_right_order() {
        // &amp; must be replaced last, or "&amp;lt;" would wrongly become "<".
        assert_eq!(html_decode("&lt;a&gt;"), "<a>");
        assert_eq!(html_decode("Simon &amp; Garfunkel"), "Simon & Garfunkel");
        assert_eq!(html_decode("&quot;x&quot; &apos;y&apos;"), "\"x\" 'y'");
        // Double-encoded entities decode exactly one level. These fail if
        // `&amp;` is replaced before `&quot;`/`&apos;`, which it was until
        // CodeRabbit caught that the code contradicted this function's own doc
        // comment - and the only prior case (`&amp;lt;`) passed by accident,
        // because `&lt;` happens to be replaced first.
        assert_eq!(html_decode("&amp;lt;"), "&lt;");
        assert_eq!(html_decode("&amp;quot;"), "&quot;");
        assert_eq!(html_decode("&amp;apos;"), "&apos;");
        assert_eq!(html_decode("&amp;gt;"), "&gt;");
    }

    #[test]
    fn parse_didl_lite_reads_bare_tags() {
        let t = parse_didl_lite(BARE_DIDL).expect("returns Some");
        assert_eq!(t.title, "Hoppipolla");
        assert_eq!(t.artist, "Sigur Ros");
        assert_eq!(t.album, "Takk...");
        assert_eq!(
            t.album_art_uri.as_deref(),
            Some("http://10.0.0.5/art/42.jpg")
        );
    }

    #[test]
    fn parse_didl_lite_never_returns_none_even_for_garbage() {
        // Every field defaults rather than failing; both adapters rely on this.
        let t = parse_didl_lite("not xml at all").expect("returns Some");
        assert_eq!(t.title, "");
        assert_eq!(t.artist, "");
        assert_eq!(t.album, "");
        assert!(t.album_art_uri.is_none());
    }

    #[test]
    fn parse_didl_lite_falls_back_to_dc_creator_and_plain_title() {
        let xml =
            r#"<item><title>Bare Title</title><dc:creator>Fallback Artist</dc:creator></item>"#;
        let t = parse_didl_lite(xml).expect("returns Some");
        assert_eq!(t.title, "Bare Title");
        assert_eq!(t.artist, "Fallback Artist");
    }

    /// Regression for the defect this module was extracted to fix: the previous
    /// matcher required a literal `<tag>`, so the DLNA-conventional
    /// `<upnp:albumArtURI dlna:profileID="JPEG_TN">` was invisible and album
    /// art silently went missing on real devices.
    #[test]
    fn parse_didl_lite_reads_tags_that_carry_attributes() {
        let t = parse_didl_lite(REAL_DIDL).expect("returns Some");
        assert_eq!(t.title, "Hoppipolla");
        assert_eq!(t.artist, "Sigur Ros");
        assert_eq!(t.album, "Takk...");
        assert_eq!(t.genre.as_deref(), Some("Post-rock"));
        assert_eq!(
            t.album_art_uri.as_deref(),
            Some("http://10.0.0.5/art/42.jpg"),
            "attribute-bearing albumArtURI must be found"
        );
    }

    #[test]
    fn extract_xml_value_does_not_match_a_longer_tag_with_the_same_prefix() {
        // `upnp:album` must not swallow `upnp:albumArtURI`.
        let xml = r#"<item><upnp:albumArtURI>art.jpg</upnp:albumArtURI><upnp:album>Real Album</upnp:album></item>"#;
        assert_eq!(
            extract_xml_value(xml, "upnp:album").as_deref(),
            Some("Real Album")
        );
    }

    #[test]
    fn extract_xml_value_skips_self_closing_tags() {
        let xml =
            r#"<item><upnp:albumArtURI /><upnp:albumArtURI>art.jpg</upnp:albumArtURI></item>"#;
        assert_eq!(
            extract_xml_value(xml, "upnp:albumArtURI").as_deref(),
            Some("art.jpg")
        );
    }

    #[test]
    fn extract_xml_value_tolerates_a_namespace_prefix_when_the_tag_has_none() {
        // SOAP responses may or may not namespace their elements; the UPnP
        // adapter's own regex extractor allowed either, and consolidating on
        // this helper must not drop that.
        let xml = r#"<s:Body><u:GetVolumeResponse><u:CurrentVolume>42</u:CurrentVolume></u:GetVolumeResponse></s:Body>"#;
        assert_eq!(
            extract_xml_value(xml, "CurrentVolume").as_deref(),
            Some("42")
        );
        let bare = r#"<Body><CurrentVolume>7</CurrentVolume></Body>"#;
        assert_eq!(
            extract_xml_value(bare, "CurrentVolume").as_deref(),
            Some("7")
        );
    }

    #[test]
    fn extract_xml_value_with_a_prefixed_tag_stays_exact() {
        // `upnp:album` must not match `upnp:albumArtURI`, and must not match a
        // differently-prefixed `album` either - DIDL-Lite callers rely on this.
        let xml = r#"<item><upnp:albumArtURI>art.jpg</upnp:albumArtURI><other:album>Wrong</other:album></item>"#;
        assert!(extract_xml_value(xml, "upnp:album").is_none());
    }

    #[test]
    fn extract_xml_value_prefix_search_ignores_non_prefix_colons() {
        // A colon inside an attribute value must not be mistaken for a prefix.
        let xml =
            r#"<item><link href="http://x/y">no</link><TrackURI>uri://real</TrackURI></item>"#;
        assert_eq!(
            extract_xml_value(xml, "TrackURI").as_deref(),
            Some("uri://real")
        );
    }

    #[test]
    fn extract_xml_value_returns_none_when_absent() {
        assert!(extract_xml_value("<item><dc:title>x</dc:title></item>", "upnp:album").is_none());
    }
}

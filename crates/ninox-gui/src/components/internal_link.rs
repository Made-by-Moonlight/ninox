//! Links into the app itself (a brain entry, a session), carried through
//! `iced::widget::markdown` as `https://ninox.invalid/<kind>/<id>`: iced
//! 0.13's `markdown::parse` drops every link whose scheme isn't http or
//! https, so a custom scheme renders as plain, unclickable text. `.invalid`
//! is reserved (RFC 2606) and never resolves, so a link that escapes to the
//! browser by mistake goes nowhere.

use iced::widget::markdown::Url;

const HOST: &str = "ninox.invalid";
const ORIGIN: &str = "https://ninox.invalid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternalLink {
    Brain(String),
    Session(String),
    /// A wikilink whose target isn't in the brain: drawn dim, never followed.
    Unresolved(String),
}

/// Everything outside RFC 3986's unreserved set is escaped, so the id is a
/// single path segment (`/` included) and the whole URL is a valid
/// CommonMark link destination (no spaces or unbalanced parens).
pub fn encode(link: &InternalLink) -> String {
    let (kind, id) = match link {
        InternalLink::Brain(id) => ("brain", id),
        InternalLink::Session(id) => ("session", id),
        InternalLink::Unresolved(id) => ("brain-missing", id),
    };
    let mut out = format!("{ORIGIN}/{kind}/");
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Any URL on our reserved host, whether or not it decodes — one that
/// doesn't (an empty `[[#heading]]` target, a hand-written path) must stay
/// inert rather than open a dead browser tab.
pub fn is_internal(url: &Url) -> bool {
    url.host_str() == Some(HOST)
}

/// `None` for any URL that isn't one of ours — route it to `OpenUrl`
/// unless [`is_internal`].
pub fn decode(url: &Url) -> Option<InternalLink> {
    if url.scheme() != "https" || !is_internal(url) {
        return None;
    }
    let mut segments = url.path_segments()?;
    let (kind, id, None) = (segments.next()?, segments.next()?, segments.next()) else {
        return None;
    };
    let id = percent_decode(id).filter(|id| !id.is_empty())?;
    match kind {
        "brain" => Some(InternalLink::Brain(id)),
        "session" => Some(InternalLink::Session(id)),
        "brain-missing" => Some(InternalLink::Unresolved(id)),
        _ => None,
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(link: InternalLink) -> Option<InternalLink> {
        decode(&Url::parse(&encode(&link)).unwrap())
    }

    #[test]
    fn brain_and_session_links_round_trip() {
        for id in [
            "frame-alignment",
            "concepts/tmux sockets.md",
            "a (parenthetical) target",
            "100% done",
            "café #2?x=1",
            "ünï/çødé",
        ] {
            assert_eq!(
                round_trip(InternalLink::Brain(id.into())),
                Some(InternalLink::Brain(id.into())),
                "{id}"
            );
            assert_eq!(
                round_trip(InternalLink::Session(id.into())),
                Some(InternalLink::Session(id.into())),
                "{id}"
            );
        }
    }

    #[test]
    fn encoded_links_are_https_on_the_reserved_host() {
        assert_eq!(
            encode(&InternalLink::Brain("concepts/a b.md".into())),
            "https://ninox.invalid/brain/concepts%2Fa%20b.md"
        );
        assert_eq!(
            encode(&InternalLink::Session("w-1".into())),
            "https://ninox.invalid/session/w-1"
        );
    }

    #[test]
    fn foreign_and_malformed_urls_are_not_internal() {
        for url in [
            "https://example.com/brain/x",
            "http://ninox.invalid/brain/x",
            "https://ninox.invalid/other/x",
            "https://ninox.invalid/brain/",
            "https://ninox.invalid/brain",
            "https://ninox.invalid/brain/a/b",
            "https://ninox.invalid/brain/%FF",
        ] {
            assert_eq!(decode(&Url::parse(url).unwrap()), None, "{url}");
        }
    }

    #[test]
    fn undecodable_links_on_the_reserved_host_are_still_internal() {
        let empty = encode(&InternalLink::Brain(String::new()));
        for url in [
            empty.as_str(),
            "https://ninox.invalid/",
            "https://ninox.invalid/brain/x/",
            "http://ninox.invalid/brain/x",
        ] {
            let url = Url::parse(url).unwrap();
            assert_eq!(decode(&url), None, "{url}");
            assert!(is_internal(&url), "{url}");
        }
        assert!(!is_internal(
            &Url::parse("https://example.com/brain/x").unwrap()
        ));
    }

    /// The bug this module exists for: a link has to survive
    /// `markdown::parse` to be clickable at all.
    #[test]
    fn an_encoded_link_survives_markdown_parse() {
        let link = InternalLink::Brain("my target (x)".into());
        let md = format!("see [it]({}) now", encode(&link));
        let style = iced::widget::markdown::Style {
            inline_code_highlight: iced::widget::markdown::Highlight {
                background: iced::Background::Color(iced::Color::BLACK),
                border: iced::Border::default(),
            },
            inline_code_padding: iced::Padding::ZERO,
            inline_code_color: iced::Color::WHITE,
            link_color: iced::Color::WHITE,
        };
        let links: Vec<Url> = iced::widget::markdown::parse(&md)
            .filter_map(|item| match item {
                iced::widget::markdown::Item::Paragraph(text) => Some(text),
                _ => None,
            })
            .flat_map(|text| {
                text.spans(style)
                    .iter()
                    .filter_map(|s| s.link.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(links.len(), 1, "{md}");
        assert_eq!(decode(&links[0]), Some(link));
    }
}

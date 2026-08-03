// Inline media: URL classification and the image texture cache.
//
// Classification is by extension over http(s) URLs — the same heuristic the
// web client uses for its inline embeds. Rendering is gated per-kind by
// device-local toggles (a phone-sized setting the server registry doesn't
// model), and images are fetched through the app's tokio runtime into a
// texture cache so redraws are free.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "svg"];
const VIDEO_EXT: &[&str] = &["mp4", "webm", "mov", "mkv", "m4v"];
const AUDIO_EXT: &[&str] = &["mp3", "ogg", "opus", "flac", "wav", "m4a", "aac"];

/// Largest image fetched for inline display. Anything bigger renders as the
/// plain link it already is.
pub const MAX_IMAGE_BYTES: usize = 15 * 1024 * 1024;

/// Classify one URL by extension, ignoring query and fragment.
pub fn classify(url: &str) -> Option<MediaKind> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    if IMAGE_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else if AUDIO_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Audio)
    } else {
        None
    }
}

/// Every media URL in a message, in order, deduped.
pub fn media_urls(text: &str) -> Vec<(String, MediaKind)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        // Strip punctuation a URL picks up in prose, both ends: a leading
        // "(" from "(https://…)" and trailing ",.;)>]".
        let url = token
            .trim_start_matches(['(', '<', '['])
            .trim_end_matches([',', '.', ';', ')', '>', ']']);
        if let Some(kind) = classify(url) {
            if seen.insert(url.to_string()) {
                out.push((url.to_string(), kind));
            }
        }
    }
    out
}

/// Find every http(s) URL in `text`, as `(start_char, end_char, url)` in
/// **character** offsets (what a GtkTextBuffer indexes by), with trailing
/// prose punctuation trimmed off the end. Used to make links clickable.
pub fn find_links(text: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // A URL starts at a word boundary with http:// or https://.
        let at_boundary = i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], '(' | '<' | '[');
        let rest: String = chars[i..].iter().take(8).collect();
        if at_boundary && (rest.starts_with("http://") || rest.starts_with("https://")) {
            let start = i;
            let mut end = i;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            // Trim trailing punctuation that a URL picks up in prose.
            while end > start && matches!(chars[end - 1], ',' | '.' | ';' | ':' | ')' | '>' | ']' | '!' | '?') {
                end -= 1;
            }
            let url: String = chars[start..end].iter().collect();
            if url.len() > "https://".len() {
                out.push((start, end, url));
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// True for a YouTube watch/short URL — the only hosts link previews fetch,
/// keeping the privacy surface (fetching URLs seen in chat) tightly bounded.
pub fn is_youtube(url: &str) -> bool {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .trim_start_matches("m.");
    matches!(host, "youtube.com" | "youtu.be")
}

/// A fetched link preview: Open Graph title and description.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    pub title: String,
    pub description: String,
    /// og:image / twitter:image, absolute URL or empty. Rendered as the
    /// card's thumbnail through the same image cache inline images use.
    pub image: String,
}

impl Preview {
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.description.is_empty()
    }
}

/// Extract Open Graph `og:title` / `og:description` (falling back to `<title>`
/// and `<meta name=description>`) from a page's HTML. Pure string scanning —
/// no HTML parser dependency, tolerant of attribute order and quote style.
pub fn extract_preview(html: &str) -> Preview {
    Preview {
        title: meta_content(html, &["og:title", "twitter:title"])
            .or_else(|| title_tag(html))
            .unwrap_or_default(),
        description: meta_content(html, &["og:description", "twitter:description", "description"])
            .unwrap_or_default(),
        // Only absolute http(s) images: a relative og:image would need the
        // page URL to resolve, and a scheme-relative one is ambiguous — both
        // are rare enough to skip rather than guess.
        image: meta_content(html, &["og:image", "twitter:image"])
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .unwrap_or_default(),
    }
}

/// Whether a URL is worth an article-card preview: http(s), and not a direct
/// media file — those get real embeds instead.
pub fn is_previewable(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://")) && classify(url).is_none()
}

/// Value of the first `<meta ... property|name="<key>" ... content="...">` for
/// any of `keys`, HTML-entity-decoded and trimmed.
fn meta_content(html: &str, keys: &[&str]) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    for key in keys {
        let mut from = 0;
        // Match the key bounded by a quote on each side, so `og:title` doesn't
        // match `og:title_extra` and either quote style works.
        while let Some(pos) = lower[from..].find(key) {
            let abs = from + pos;
            let before = lower.as_bytes().get(abs.wrapping_sub(1)).copied();
            let after = lower.as_bytes().get(abs + key.len()).copied();
            let quoted = matches!(before, Some(b'"' | b'\''))
                && matches!(after, Some(b'"' | b'\''));
            if !quoted {
                from = abs + key.len();
                continue;
            }
            // Look at the surrounding tag (bounded window) for a content attr.
            let tag_start = lower[..abs].rfind('<').unwrap_or(abs);
            let tag_end = lower[abs..].find('>').map(|e| abs + e).unwrap_or(html.len());
            let tag = &html[tag_start..tag_end.min(html.len())];
            if let Some(content) = attr(tag, "content") {
                let decoded = decode_entities(&content);
                let trimmed = decoded.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            from = tag_end.max(abs + key.len());
        }
    }
    None
}

fn title_tag(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open = lower[start..].find('>').map(|o| start + o + 1)?;
    let end = lower[open..].find("</title>").map(|e| open + e)?;
    let text = decode_entities(html[open..end].trim());
    (!text.is_empty()).then_some(text)
}

/// Value of `name="..."` / `name='...'` in a tag fragment.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(name)?;
    let after = &tag[at + name.len()..];
    let after = after.trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let (quote, rest) = match after.chars().next()? {
        q @ ('"' | '\'') => (q, &after[1..]),
        _ => return None,
    };
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Decode the handful of HTML entities common in og:description text.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "\'")
        .replace("&apos;", "\'")
        .replace("&#x27;", "\'")
}

/// Image texture cache: URL → decoded texture, plus the in-flight set so a
/// URL is fetched once no matter how many renders ask for it.
#[derive(Default)]
pub struct ImageCache {
    textures: RefCell<HashMap<String, gtk::gdk::Texture>>,
    /// URLs currently downloading.
    inflight: RefCell<HashSet<String>>,
    /// URLs that failed — retried never, rendered as plain links.
    failed: RefCell<HashSet<String>>,
}

impl ImageCache {
    pub fn get(&self, url: &str) -> Option<gtk::gdk::Texture> {
        self.textures.borrow().get(url).cloned()
    }

    pub fn is_failed(&self, url: &str) -> bool {
        self.failed.borrow().contains(url)
    }

    /// Mark a fetch started. Returns false if one is already running or the
    /// URL previously failed.
    pub fn begin(&self, url: &str) -> bool {
        if self.failed.borrow().contains(url) || self.textures.borrow().contains_key(url) {
            return false;
        }
        self.inflight.borrow_mut().insert(url.to_string())
    }

    pub fn complete(&self, url: &str, bytes: &[u8]) -> bool {
        self.inflight.borrow_mut().remove(url);
        match gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(bytes)) {
            Ok(texture) => {
                self.textures.borrow_mut().insert(url.to_string(), texture);
                true
            }
            Err(_) => {
                self.failed.borrow_mut().insert(url.to_string());
                false
            }
        }
    }

    /// Whether a fetch for this url is in flight, so the renderer can say
    /// "loading" instead of showing nothing at all.
    pub fn is_loading(&self, url: &str) -> bool {
        self.inflight.borrow().contains(url)
    }

    /// Forget a failure so the next render retries it.
    pub fn clear_failure(&self, url: &str) {
        self.failed.borrow_mut().remove(url);
    }

    pub fn fail(&self, url: &str) {
        self.inflight.borrow_mut().remove(url);
        self.failed.borrow_mut().insert(url.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(classify("https://x.example/a.PNG"), Some(MediaKind::Image));
        assert_eq!(classify("https://cdn.lurker.chat/j5qv1WgVRrkN.webp"), Some(MediaKind::Image));
        assert_eq!(classify("https://x.example/clip.mp4"), Some(MediaKind::Video));
        assert_eq!(classify("https://x.example/song.flac"), Some(MediaKind::Audio));
        assert_eq!(classify("https://x.example/page.html"), None);
        assert_eq!(classify("ftp://x.example/a.png"), None, "http(s) only");
        assert_eq!(classify("a.png"), None, "bare filenames are not links");
    }

    #[test]
    fn query_strings_and_fragments_do_not_hide_the_extension() {
        assert_eq!(classify("https://x.example/a.png?size=big#frag"), Some(MediaKind::Image));
    }

    #[test]
    fn extracts_urls_from_prose_with_trailing_punctuation() {
        let found = media_urls("look at https://x.example/cat.webp, amazing (https://x.example/dog.mp4)");
        assert_eq!(
            found,
            [
                ("https://x.example/cat.webp".to_string(), MediaKind::Image),
                ("https://x.example/dog.mp4".to_string(), MediaKind::Video),
            ]
        );
    }

    #[test]
    fn finds_links_with_char_offsets_and_trims_punctuation() {
        let text = "see https://example.com/a, and (http://b.test) end";
        let links = find_links(text);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].2, "https://example.com/a");
        assert_eq!(links[1].2, "http://b.test");
        // Offsets index the URL substring, punctuation excluded.
        let chars: Vec<char> = text.chars().collect();
        let (s0, e0, _) = &links[0];
        assert_eq!(chars[*s0..*e0].iter().collect::<String>(), "https://example.com/a");
    }

    #[test]
    fn find_links_handles_unicode_before_the_url() {
        // Char offsets, not byte offsets — a multi-byte prefix must not
        // misalign the range.
        let text = "café https://example.com";
        let links = find_links(text);
        assert_eq!(links.len(), 1);
        let chars: Vec<char> = text.chars().collect();
        let (s, e, url) = &links[0];
        assert_eq!(chars[*s..*e].iter().collect::<String>(), *url);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn ignores_non_http_and_mid_word_matches() {
        assert!(find_links("ftp://x.example/f").is_empty());
        assert!(find_links("emailhttps://nope.com").is_empty(), "must start at a boundary");
    }

    #[test]
    fn detects_youtube_hosts_only() {
        assert!(is_youtube("https://www.youtube.com/watch?v=abc"));
        assert!(is_youtube("https://youtu.be/abc"));
        assert!(is_youtube("http://m.youtube.com/watch?v=x"));
        assert!(!is_youtube("https://example.com/watch?v=abc"));
        assert!(!is_youtube("https://notyoutube.com/x"));
    }

    #[test]
    fn extracts_open_graph_title_and_description() {
        let html = r#"<html><head>
            <meta property="og:title" content="Never Gonna Give You Up">
            <meta property="og:description" content="Rick Astley&#39;s official video &amp; more">
            </head></html>"#;
        let p = extract_preview(html);
        assert_eq!(p.title, "Never Gonna Give You Up");
        assert_eq!(p.description, "Rick Astley's official video & more");
    }

    #[test]
    fn falls_back_to_title_tag_and_meta_description() {
        let html = r#"<head><title>Plain Title</title>
            <meta name="description" content="a description"></head>"#;
        let p = extract_preview(html);
        assert_eq!(p.title, "Plain Title");
        assert_eq!(p.description, "a description");
    }

    #[test]
    fn tolerates_single_quotes_and_attribute_order() {
        let html = "<meta content='Quoted Title' property='og:title'>";
        assert_eq!(extract_preview(html).title, "Quoted Title");
    }

    #[test]
    fn empty_when_no_metadata() {
        assert!(extract_preview("<html><body>nothing</body></html>").is_empty());
    }

    #[test]
    fn duplicate_urls_embed_once() {
        let found = media_urls("https://x.example/a.png and again https://x.example/a.png");
        assert_eq!(found.len(), 1);
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn cards_extract_title_description_and_thumbnail() {
        let html = r#"<html><head>
            <meta property="og:title" content="Ergo IRC daemon">
            <meta name="og:description" content="A modern ircd.">
            <meta property="og:image" content="https://example.com/card.png">
            </head></html>"#;
        let p = extract_preview(html);
        assert_eq!(p.title, "Ergo IRC daemon");
        assert_eq!(p.description, "A modern ircd.");
        assert_eq!(p.image, "https://example.com/card.png");
    }

    #[test]
    fn relative_or_odd_scheme_thumbnails_are_dropped() {
        // A relative og:image needs the page URL to resolve; guessing wrong
        // fetches garbage. Skip rather than guess.
        for src in ["/card.png", "//cdn.example.com/card.png", "data:image/png;base64,x"] {
            let html = format!(r#"<meta property="og:image" content="{src}">"#);
            assert_eq!(extract_preview(&html).image, "", "{src}");
        }
    }

    #[test]
    fn previewable_is_http_non_media_only() {
        assert!(is_previewable("https://ergo.chat/about"));
        assert!(is_previewable("http://example.com"));
        assert!(!is_previewable("https://example.com/clip.mp4"), "media gets real embeds");
        assert!(!is_previewable("https://example.com/pic.jpg"));
        assert!(!is_previewable("ftp://example.com/file"), "http(s) only");
        assert!(!is_previewable("mailto:someone@example.com"));
    }
}

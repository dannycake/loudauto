//! Liked Songs and search results arrive as a resolved playback context.

use std::future::Future;

use librespot_protocol::{context::Context, context_page::ContextPage};

use crate::{Result, SpotError, model::LibraryEntry};

/// Upper bound on fetched context pages, in case a server keeps returning a next page.
pub(crate) const MAX_PAGE_FETCHES: usize = 200;

/// Every entry of a resolved context, including pages that must be fetched: placeholder
/// pages (a `page_url` but no tracks) and `next_page_url` chains. `fetch` returns the
/// JSON body for one page URL. An empty URL string means "no page".
pub(crate) async fn collect_entries<F, Fut>(ctx: &Context, fetch: F) -> Result<Vec<LibraryEntry>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let mut fetch = fetch;
    let mut entries = Vec::new();
    let mut fetches = 0;
    for listed in &ctx.pages {
        let mut page = std::borrow::Cow::Borrowed(listed);
        if listed.tracks.is_empty()
            && let Some(url) = non_empty(&listed.page_url)
        {
            if fetches == MAX_PAGE_FETCHES {
                log::warn!("context: stopped after {MAX_PAGE_FETCHES} page fetches");
                return Ok(entries);
            }
            fetches += 1;
            page = std::borrow::Cow::Owned(fetch_page(&mut fetch, url).await?);
        }
        entries.extend(entries_from_page(&page));

        let mut next = non_empty(&page.next_page_url);
        while let Some(url) = next {
            if fetches == MAX_PAGE_FETCHES {
                log::warn!("context: stopped after {MAX_PAGE_FETCHES} page fetches");
                return Ok(entries);
            }
            fetches += 1;
            let fetched = fetch_page(&mut fetch, url).await?;
            entries.extend(entries_from_page(&fetched));
            next = non_empty(&fetched.next_page_url);
        }
    }
    Ok(entries)
}

/// Spotify uses an empty string as well as absence to mean "no page" (librespot's
/// connect code treats an empty `page_url` the same way).
fn non_empty(url: &Option<String>) -> Option<String> {
    url.as_deref().filter(|u| !u.is_empty()).map(str::to_string)
}

/// Fetch and parse one page. A fetched page without tracks is an error rather than a
/// silent stop: it is also what a wrong-shaped body parses to.
async fn fetch_page<F, Fut>(fetch: &mut F, url: String) -> Result<ContextPage>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let body = fetch(url.clone()).await?;
    let page = parse_page_json(&body)?;
    if page.tracks.is_empty() {
        return Err(SpotError::UnexpectedPage(format!("{url} (no tracks)")));
    }
    Ok(page)
}

/// Next-page bodies come back as JSON `ContextPage`s (via librespot's `request_as_json`).
fn parse_page_json(bytes: &[u8]) -> Result<ContextPage> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| SpotError::Protobuf(protobuf::Error::from(std::io::Error::other(e))))?;
    let options = protobuf_json_mapping::ParseOptions {
        ignore_unknown_fields: true,
        ..Default::default()
    };
    protobuf_json_mapping::parse_from_str_with_options::<ContextPage>(text, &options)
        .map_err(|e| SpotError::Protobuf(protobuf::Error::from(std::io::Error::other(e))))
}

/// Flatten every page of a resolved context into entries, in order.
pub fn entries_from_context(ctx: &Context) -> Vec<LibraryEntry> {
    ctx.pages.iter().flat_map(entries_from_page).collect()
}

/// Entries from one page, in order; tracks without a URI are skipped.
pub(crate) fn entries_from_page(page: &ContextPage) -> Vec<LibraryEntry> {
    page.tracks
        .iter()
        .filter_map(|track| {
            let uri = track.uri.clone()?;
            // Unix milliseconds as a decimal string, e.g. "1793681473000".
            let added_at_ms = track.metadata.get("added_at").and_then(|s| s.parse().ok());
            Some(LibraryEntry { uri, added_at_ms })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;
    use crate::SpotError;
    use librespot_protocol::context_track::ContextTrack;

    fn track(uri: Option<&str>, added_at: Option<&str>) -> ContextTrack {
        let mut t = ContextTrack::new();
        t.uri = uri.map(str::to_string);
        if let Some(v) = added_at {
            t.metadata.insert("added_at".into(), v.into());
        }
        t
    }

    fn context(pages: Vec<Vec<ContextTrack>>) -> Context {
        let mut ctx = Context::new();
        for tracks in pages {
            let mut page = ContextPage::new();
            page.tracks = tracks;
            ctx.pages.push(page);
        }
        ctx
    }

    fn entry(uri: &str, added_at_ms: Option<i64>) -> LibraryEntry {
        LibraryEntry {
            uri: uri.into(),
            added_at_ms,
        }
    }

    #[test]
    fn liked_entries_with_dates() {
        let ctx = context(vec![vec![
            track(
                Some("spotify:track:1XD4K4CGAKTIBmFpvuaFru"),
                Some("1793681473000"),
            ),
            track(Some("spotify:local:Artist:Album:Title:180"), None),
        ]]);
        assert_eq!(
            entries_from_context(&ctx),
            vec![
                entry("spotify:track:1XD4K4CGAKTIBmFpvuaFru", Some(1793681473000)),
                entry("spotify:local:Artist:Album:Title:180", None),
            ]
        );
    }

    #[test]
    fn multiple_pages_concatenate() {
        let ctx = context(vec![
            vec![track(Some("spotify:track:a"), None)],
            vec![track(Some("spotify:track:b"), None)],
        ]);
        assert_eq!(
            entries_from_context(&ctx),
            vec![
                entry("spotify:track:a", None),
                entry("spotify:track:b", None)
            ]
        );
    }

    #[test]
    fn bad_added_at_is_none() {
        let ctx = context(vec![vec![track(
            Some("spotify:track:a"),
            Some("yesterday"),
        )]]);
        assert_eq!(
            entries_from_context(&ctx),
            vec![entry("spotify:track:a", None)]
        );
    }

    #[test]
    fn missing_uri_skipped() {
        let ctx = context(vec![vec![
            track(None, Some("1")),
            track(Some("spotify:track:a"), None),
        ]]);
        assert_eq!(
            entries_from_context(&ctx),
            vec![entry("spotify:track:a", None)]
        );
    }

    fn page(uris: &[&str], page_url: &str, next: Option<&str>) -> ContextPage {
        let mut p = ContextPage::new();
        p.tracks = uris.iter().map(|u| track(Some(u), None)).collect();
        if !page_url.is_empty() {
            p.page_url = Some(page_url.into());
        }
        p.next_page_url = next.map(str::to_string);
        p
    }

    fn json(p: &ContextPage) -> Vec<u8> {
        protobuf_json_mapping::print_to_string(p)
            .unwrap()
            .into_bytes()
    }

    fn ctx_of(pages: Vec<ContextPage>) -> Context {
        let mut ctx = Context::new();
        ctx.pages = pages;
        ctx
    }

    fn uris(entries: &[LibraryEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.uri.as_str()).collect()
    }

    /// A fetcher that serves canned pages and records every URL it was asked for.
    struct Server {
        pages: HashMap<String, Vec<u8>>,
        asked: RefCell<Vec<String>>,
    }

    impl Server {
        fn new(pages: &[(&str, ContextPage)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(u, p)| ((*u).to_string(), json(p)))
                    .collect(),
                asked: RefCell::new(Vec::new()),
            }
        }
        fn fetch(&self, url: String) -> impl Future<Output = Result<Vec<u8>>> + '_ {
            self.asked.borrow_mut().push(url.clone());
            let body = self
                .pages
                .get(&url)
                .cloned()
                .unwrap_or_else(|| panic!("unexpected fetch {url}"));
            async move { Ok(body) }
        }
    }

    #[tokio::test]
    async fn empty_next_page_url_is_end() {
        let server = Server::new(&[]);
        let ctx = ctx_of(vec![page(&["spotify:track:a"], "", Some(""))]);
        let got = collect_entries(&ctx, |u| server.fetch(u)).await.unwrap();
        assert_eq!(uris(&got), ["spotify:track:a"]);
        assert!(server.asked.borrow().is_empty());
    }

    #[tokio::test]
    async fn follows_next_page_chain() {
        let server = Server::new(&[
            ("hm://p2", page(&["spotify:track:b"], "", Some("hm://p3"))),
            ("hm://p3", page(&["spotify:track:c"], "", None)),
        ]);
        let ctx = ctx_of(vec![page(&["spotify:track:a"], "", Some("hm://p2"))]);
        let got = collect_entries(&ctx, |u| server.fetch(u)).await.unwrap();
        assert_eq!(
            uris(&got),
            ["spotify:track:a", "spotify:track:b", "spotify:track:c"]
        );
    }

    #[tokio::test]
    async fn fetches_placeholder_pages() {
        let server = Server::new(&[("hm://ph", page(&["spotify:track:x"], "", None))]);
        let ctx = ctx_of(vec![page(&[], "hm://ph", None)]);
        let got = collect_entries(&ctx, |u| server.fetch(u)).await.unwrap();
        assert_eq!(uris(&got), ["spotify:track:x"]);
        assert_eq!(*server.asked.borrow(), ["hm://ph"]);
    }

    #[tokio::test]
    async fn fetched_empty_page_is_error() {
        // Also what a wrong-shaped body becomes once unknown fields are ignored.
        let server = Server::new(&[("hm://p2", page(&[], "", None))]);
        let ctx = ctx_of(vec![page(&["spotify:track:a"], "", Some("hm://p2"))]);
        let got = collect_entries(&ctx, |u| server.fetch(u)).await;
        assert!(matches!(got, Err(SpotError::UnexpectedPage(u)) if u == "hm://p2 (no tracks)"));
    }

    #[tokio::test]
    async fn stops_at_page_cap() {
        let server = Server::new(&[(
            "hm://loop",
            page(&["spotify:track:t"], "", Some("hm://loop")),
        )]);
        let ctx = ctx_of(vec![page(&["spotify:track:a"], "", Some("hm://loop"))]);
        let got = collect_entries(&ctx, |u| server.fetch(u)).await.unwrap();
        assert_eq!(server.asked.borrow().len(), MAX_PAGE_FETCHES);
        assert_eq!(got.len(), 1 + MAX_PAGE_FETCHES);
    }
}

//! The user's playlist sidebar ("rootlist"), parsed from the raw `playlist4_external`
//! protobuf. librespot-metadata's typed `SelectedListContent::try_from` is avoided on
//! purpose: it fails the whole list with `InvalidId` on entries it can't map.

use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::Message;

use crate::{
    Result,
    model::{PlaylistRef, RootEntry},
};

/// Parse the body of `SpClient::get_rootlist`.
pub fn parse_rootlist(bytes: &[u8]) -> Result<Vec<RootEntry>> {
    let msg = SelectedListContent::parse_from_bytes(bytes)?;
    let contents = msg.contents.get_or_default();
    // Names come from meta_items, which Spotify returns index-aligned with items when
    // the request asks for `decorate=attributes`. If the counts differ, the pairing is
    // unknowable, so leave names empty rather than mislabel playlists.
    let aligned = contents.meta_items.len() == contents.items.len();
    let entries = contents
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let name = if aligned {
                contents.meta_items[i]
                    .attributes
                    .get_or_default()
                    .name()
                    .to_string()
            } else {
                String::new()
            };
            classify(item.uri(), name)
        })
        .collect();
    Ok(entries)
}

fn classify(uri: &str, name: String) -> RootEntry {
    if let Some(rest) = uri.strip_prefix("spotify:start-group:") {
        // spotify:start-group:<id>:<name, '+' for spaces, percent-encoded>
        let (id, raw_name) = rest.split_once(':').unwrap_or((rest, ""));
        return RootEntry::FolderStart {
            id: id.to_string(),
            name: decode_folder_name(raw_name),
        };
    }
    if let Some(id) = uri.strip_prefix("spotify:end-group:") {
        return RootEntry::FolderEnd { id: id.to_string() };
    }
    if uri.starts_with("spotify:playlist:")
        || (uri.starts_with("spotify:user:") && uri.contains(":playlist:"))
    {
        return RootEntry::Playlist(PlaylistRef {
            uri: uri.to_string(),
            name,
        });
    }
    RootEntry::Other {
        uri: uri.to_string(),
    }
}

fn decode_folder_name(raw: &str) -> String {
    let spaced = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&spaced)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SpotError, model::PlaylistRef};
    use librespot_protocol::playlist4_external::{
        Item, ListAttributes, ListItems, MetaItem, SelectedListContent,
    };
    use protobuf::Message;

    /// Serialize a rootlist with these item URIs and (optionally) meta names.
    fn fixture(uris: &[&str], names: &[&str]) -> Vec<u8> {
        let mut contents = ListItems::new();
        contents.set_pos(0);
        contents.set_truncated(false);
        for uri in uris {
            let mut item = Item::new();
            item.set_uri((*uri).to_string());
            contents.items.push(item);
        }
        for name in names {
            let mut attrs = ListAttributes::new();
            attrs.set_name((*name).to_string());
            let mut meta = MetaItem::new();
            meta.attributes = protobuf::MessageField::some(attrs);
            contents.meta_items.push(meta);
        }
        let mut msg = SelectedListContent::new();
        msg.contents = protobuf::MessageField::some(contents);
        msg.write_to_bytes().expect("fixture serializes")
    }

    fn playlist(uri: &str, name: &str) -> RootEntry {
        RootEntry::Playlist(PlaylistRef {
            uri: uri.into(),
            name: name.into(),
        })
    }

    #[test]
    fn parses_playlists_with_names() {
        let bytes = fixture(
            &[
                "spotify:playlist:21erFbLzdJurEYKz86wDwv",
                "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi",
            ],
            &["New Blood", "Main"],
        );
        assert_eq!(
            parse_rootlist(&bytes).unwrap(),
            vec![
                playlist("spotify:playlist:21erFbLzdJurEYKz86wDwv", "New Blood"),
                playlist("spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi", "Main"),
            ]
        );
    }

    #[test]
    fn parses_folders() {
        let bytes = fixture(
            &[
                "spotify:start-group:abc123:Old+Stuff%21",
                "spotify:playlist:20Lj4Gb8SAVFfBfALkBbiv",
                "spotify:end-group:abc123",
            ],
            &["", "2022", ""],
        );
        assert_eq!(
            parse_rootlist(&bytes).unwrap(),
            vec![
                RootEntry::FolderStart {
                    id: "abc123".into(),
                    name: "Old Stuff!".into()
                },
                playlist("spotify:playlist:20Lj4Gb8SAVFfBfALkBbiv", "2022"),
                RootEntry::FolderEnd {
                    id: "abc123".into()
                },
            ]
        );
    }

    #[test]
    fn unknown_uris_are_other() {
        let bytes = fixture(&["spotify:collection:whatever", "garbage"], &[]);
        assert_eq!(
            parse_rootlist(&bytes).unwrap(),
            vec![
                RootEntry::Other {
                    uri: "spotify:collection:whatever".into()
                },
                RootEntry::Other {
                    uri: "garbage".into()
                },
            ]
        );
    }

    #[test]
    fn names_empty_when_meta_misaligned() {
        let bytes = fixture(
            &[
                "spotify:playlist:21erFbLzdJurEYKz86wDwv",
                "spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi",
            ],
            &["New Blood"],
        );
        assert_eq!(
            parse_rootlist(&bytes).unwrap(),
            vec![
                playlist("spotify:playlist:21erFbLzdJurEYKz86wDwv", ""),
                playlist("spotify:playlist:3ld2R8VhdMN1Y1CwdlXAAi", ""),
            ]
        );
    }

    #[test]
    fn bad_bytes_error() {
        assert!(matches!(
            parse_rootlist(&[0xff, 0xff, 0xff]),
            Err(SpotError::Protobuf(_))
        ));
    }
}

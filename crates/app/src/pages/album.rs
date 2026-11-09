//! An album: its cover, name, artists (links), year and label, then its tracks, which
//! play in the album's context. The rows leave their covers out: each would repeat the
//! one in the header.

use eframe::egui::{RichText, Ui};
use engine_ipc::{Page, PageReply, PageRequest};

use super::{List, PageCx, header, link, other_page, title, track_list};
use crate::theme;

pub fn show(ui: &mut Ui, reply: &PageReply, cx: &mut PageCx) {
    let Page::Album(album) = &reply.page else {
        return other_page(ui);
    };
    header(ui, &album.images, cx, |ui, cx| {
        ui.label(RichText::new("Album").color(theme::SECONDARY));
        title(ui, &album.name);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (i, artist) in album.artists.iter().enumerate() {
                if i > 0 {
                    ui.label(", ");
                }
                if artist.uri.starts_with("spotify:artist:") {
                    let to = PageRequest::Artist {
                        uri: artist.uri.clone(),
                    };
                    link(ui, &artist.name, to, cx);
                } else {
                    ui.label(&artist.name);
                }
            }
        });
        let meta = meta_line(&album.release_date, &album.label);
        if !meta.is_empty() {
            ui.label(RichText::new(meta).color(theme::SECONDARY));
        }
    });
    ui.add_space(16.0);
    let list = List {
        rows: &album.tracks,
        context: Some(&album.uri),
        thumbs: false,
    };
    track_list(ui, reply, list, false, cx);
}

/// `2025 · deadAir`: the release's year and its label, either left out when unknown.
pub fn meta_line(release_date: &str, label: &str) -> String {
    let year = release_date.get(..4).unwrap_or(release_date);
    [year, label.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

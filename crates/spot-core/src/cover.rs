//! Album art lives on Spotify's image CDN, addressed by the image file id in hex.

/// `https://i.scdn.co/image/<file id hex>`
pub fn cover_url(file_id_hex: &str) -> String {
    format!("https://i.scdn.co/image/{file_id_hex}")
}

/// URL of the widest cover among `(width, file_id_hex)` pairs.
pub fn largest_cover(covers: impl IntoIterator<Item = (i32, String)>) -> Option<String> {
    covers
        .into_iter()
        .max_by_key(|(width, _)| *width)
        .map(|(_, id)| cover_url(&id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_cover_picks_widest() {
        let covers = vec![
            (64, "aa".to_string()),
            (640, "cc".to_string()),
            (300, "bb".to_string()),
        ];
        assert_eq!(
            largest_cover(covers),
            Some("https://i.scdn.co/image/cc".to_string())
        );
    }

    #[test]
    fn largest_cover_of_none_is_none() {
        assert_eq!(largest_cover(Vec::new()), None);
    }

    #[test]
    fn cover_url_format() {
        assert_eq!(
            cover_url("ab67616d0000b273"),
            "https://i.scdn.co/image/ab67616d0000b273"
        );
    }
}

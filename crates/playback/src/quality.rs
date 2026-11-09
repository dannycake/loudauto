//! Soloist logs the codec of every track it opens, e.g.
//! `    codec: flac` / `    bitrate: 1400000 bits/s` / `    sample rate: 44100 Hz`.
//! Reading our own child process's log is how the UI can show "FLAC". It also logs
//! `client expires in N days` at startup, for the expiry warning.

use crate::event::StreamQuality;

#[derive(Debug, Default)]
pub struct QualityParser {
    codec: Option<String>,
    bitrate_bps: Option<u32>,
}

impl QualityParser {
    /// Feed one log line; returns the quality once a full block has been seen.
    pub fn feed(&mut self, line: &str) -> Option<StreamQuality> {
        let clean = strip_ansi(line);
        let msg = message(&clean);
        if let Some(codec) = msg.strip_prefix("codec:") {
            self.codec = Some(codec.trim().to_string());
            self.bitrate_bps = None;
        } else if let Some(rest) = msg.strip_prefix("bitrate:") {
            // Only the block's `bits/s` line; "file average bitrate: N" is a different line.
            if self.codec.is_some() {
                self.bitrate_bps = rest
                    .trim()
                    .strip_suffix("bits/s")
                    .and_then(|n| n.trim().parse().ok());
            }
        } else if let Some(rest) = msg.strip_prefix("sample rate:") {
            let rate = rest
                .trim()
                .strip_suffix("Hz")
                .and_then(|n| n.trim().parse().ok());
            if let (Some(codec), Some(bitrate_bps), Some(sample_rate_hz)) =
                (self.codec.take(), self.bitrate_bps.take(), rate)
            {
                return Some(StreamQuality {
                    codec,
                    bitrate_bps,
                    sample_rate_hz,
                });
            }
        }
        None
    }
}

/// Days left from Soloist's `client expires in N days` line (`1 day` on the last one).
pub fn parse_expiry(line: &str) -> Option<u32> {
    let clean = strip_ansi(line);
    let (days, unit) = message(&clean)
        .strip_prefix("client expires in ")?
        .split_once(' ')?;
    if !matches!(unit.trim(), "day" | "days") {
        return None;
    }
    days.parse().ok()
}

/// The message of a log line, which follows the `[f:NNN    ]` source tag.
fn message(clean: &str) -> &str {
    clean.rsplit_once(']').map_or(clean, |(_, m)| m).trim()
}

/// Remove `ESC [ ... <letter>` terminal colour sequences.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<(usize, StreamQuality)> {
        let mut p = QualityParser::default();
        lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| p.feed(l).map(|q| (i, q)))
            .collect()
    }

    #[test]
    fn parses_live_lines() {
        let lines = [
            "20:19:59.979 I [f:347                           ]     codec: flac\x1b[0m",
            "20:19:59.979 I [f:348                           ]     bitrate: 1400000 bits/s\x1b[0m",
            "20:19:59.979 I [f:349                           ]     sample rate: 44100 Hz\x1b[0m",
            "20:19:59.979 I [f:350                           ]     sample format: 32-bit-float-pcm\x1b[0m",
        ];
        assert_eq!(
            run(&lines),
            vec![(
                2,
                StreamQuality {
                    codec: "flac".into(),
                    bitrate_bps: 1_400_000,
                    sample_rate_hz: 44_100
                }
            )]
        );
    }

    #[test]
    fn vorbis_block() {
        let lines = [
            "20:16:48.115 I [f:347                           ]     codec: vorbis\x1b[0m",
            "20:16:48.115 I [f:348                           ]     bitrate: 160000 bits/s\x1b[0m",
            "20:16:48.115 I [f:349                           ]     sample rate: 44100 Hz\x1b[0m",
        ];
        assert_eq!(
            run(&lines),
            vec![(
                2,
                StreamQuality {
                    codec: "vorbis".into(),
                    bitrate_bps: 160_000,
                    sample_rate_hz: 44_100
                }
            )]
        );
    }

    #[test]
    fn parse_expiry_cases() {
        let live =
            "00:17:30.863 I [f:128                           ] client expires in 89 days\x1b[0m";
        assert_eq!(parse_expiry(live), Some(89));
        assert_eq!(parse_expiry("client expires in 1 day"), Some(1));
        assert_eq!(parse_expiry("garbage"), None);
        assert_eq!(parse_expiry(""), None);
        assert_eq!(parse_expiry("client expires in many days"), None);
        assert_eq!(parse_expiry("client expires in 89 weeks"), None);
        assert_eq!(
            parse_expiry("20:19:59.979 I [f:347                           ]     codec: flac"),
            None
        );
    }

    #[test]
    fn incomplete_block_yields_nothing() {
        let lines = [
            "20:16:48.115 I [f:347                           ]     codec: vorbis\x1b[0m",
            "20:16:48.117 I [f:722                           ] Starting playback.\x1b[0m",
            "20:16:48.114 I [f:134                           ] FileStreamer file average bitrate: 160000\x1b[0m",
        ];
        assert!(run(&lines).is_empty());
    }
}

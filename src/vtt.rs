//! SRT to WebVTT. The Chromecast receiver only loads side-loaded text tracks as WebVTT.

/// Removes `{\an8}`-style override blocks and `<font ...>` tags, which WebVTT has no use for.
/// `<i>`, `<b>` and `<u>` are valid WebVTT and kept.
fn strip_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        if rest.starts_with("{\\") {
            match rest.find('}') {
                Some(end) => rest = &rest[end + 1..],
                None => break,
            }
        } else if rest.len() >= 5
            && (rest.as_bytes()[..5].eq_ignore_ascii_case(b"<font")
                || rest.as_bytes()[..5].eq_ignore_ascii_case(b"</fon"))
        {
            match rest.find('>') {
                Some(end) => rest = &rest[end + 1..],
                None => break,
            }
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

/// Input must already be UTF-8 (see `api::to_utf8`).
pub fn srt_to_vtt(srt: &str) -> String {
    let mut out = String::with_capacity(srt.len() + 16);
    out.push_str("WEBVTT\n\n");
    for line in srt.replace("\r\n", "\n").replace('\r', "\n").lines() {
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(&strip_markup(line));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_timestamps_and_adds_header() {
        let srt = "1\r\n00:00:01,500 --> 00:00:03,000\r\nHello <i>there</i>\r\n\r\n";
        let vtt = srt_to_vtt(srt);
        assert_eq!(vtt, "WEBVTT\n\n1\n00:00:01.500 --> 00:00:03.000\nHello <i>there</i>\n\n");
    }

    #[test]
    fn strips_ass_overrides_and_font_tags() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n{\\an8}<font color=\"#ff0000\">Top, red</font>\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("\nTop, red\n"));
        assert!(!vtt.contains('{') && !vtt.contains("<font"));
    }

    #[test]
    fn keeps_commas_in_text_and_non_ascii() {
        let vtt = srt_to_vtt("1\n00:00:01,000 --> 00:00:02,000\nBlåbær, tak\n");
        assert!(vtt.contains("Blåbær, tak"));
    }
}

use super::*;

fn escaped(text: &str) -> String {
    let mut out = String::new();
    write_escaped(&mut out, text).unwrap();
    out
}

fn escaped_keeping_layout(text: &str) -> String {
    let mut out = String::new();
    write_escaped_keeping_layout(&mut out, text).unwrap();
    out
}

#[skuld::test]
fn every_control_and_direction_character_is_written_as_its_code_point() {
    let escaped_set = (0u32..=0x9f)
        .filter(|c| !(0x20..0x7f).contains(c))
        .chain(0x202a..=0x202e)
        .chain(0x2066..=0x2069)
        .chain([0x2028, 0x2029]);
    for code in escaped_set {
        let c = char::from_u32(code).unwrap();
        assert_eq!(escaped(&c.to_string()), format!("\\u{{{code:x}}}"), "U+{code:04X}");
    }
}

#[skuld::test]
fn text_outside_that_set_is_written_as_it_is() {
    // Format characters that are text: Persian ZWNJ, emoji ZWJ, soft hyphen, the marks that do not
    // reorder, the isolate-adjacent U+2065; a backslash and a Windows path.
    for text in [
        "می\u{200c}خواهم",
        "👩\u{200d}💻",
        "co\u{ad}operate",
        "a\u{200e}b\u{200f}",
        "\u{2065}",
        r"C:\Users\new\u{1b}\notes.txt",
        "plain `text`, ünïcode ✓",
        "",
    ] {
        assert_eq!(escaped(text), text);
    }
}

#[skuld::test]
fn text_already_escaped_is_not_changed_by_escaping_again() {
    let once = escaped("a\nb\u{202e}");
    assert_eq!(once, r"a\u{a}b\u{202e}");
    assert_eq!(escaped(&once), once);
}

#[skuld::test]
fn layout_mode_keeps_a_line_feed_and_a_tab_and_escapes_the_rest_of_the_set() {
    assert_eq!(
        escaped_keeping_layout("a\tb\nc\r\u{1b}[0m\u{85}\u{2028}\u{202e}"),
        concat!("a\tb\nc", r"\u{d}\u{1b}[0m\u{85}\u{2028}\u{202e}")
    );
}

/// A CR that ends a line is part of its layout (sc.exe writes CRLF); one anywhere else moves the
/// cursor.
#[skuld::test]
fn layout_mode_keeps_a_carriage_return_only_before_a_line_feed() {
    assert_eq!(escaped_keeping_layout("a\r\nb\r"), concat!("a\r\nb", r"\u{d}"));
    assert_eq!(escaped_keeping_layout("\r\r\n"), concat!(r"\u{d}", "\r\n"));
    assert_eq!(escaped_keeping_layout("a\rb"), r"a\u{d}b");
}

#[cfg(feature = "cli")]
/// What `EscapedStream` writes for `chunks`, each handed to its own `write` call.
fn streamed(chunks: &[&[u8]]) -> Vec<u8> {
    let mut sink = Vec::new();
    {
        let mut stream = EscapedStream::new(&mut sink);
        for chunk in chunks {
            std::io::Write::write_all(&mut stream, chunk).unwrap();
        }
    }
    sink
}

#[cfg(feature = "cli")]
#[skuld::test]
fn a_stream_keeps_layout_and_escapes_the_rest_of_the_set() {
    assert_eq!(
        streamed(&[b"a\tb\nc\r\x1b[0m"]),
        concat!("a\tb\nc", r"\u{d}\u{1b}[0m").as_bytes()
    );
}

#[cfg(feature = "cli")]
#[skuld::test]
fn a_character_split_across_writes_comes_out_whole() {
    let text = "d\u{e9}j\u{e0} vu \u{1f469}";
    for cut in 1..text.len() {
        if !text.is_char_boundary(cut) {
            let (head, tail) = text.as_bytes().split_at(cut);
            assert_eq!(streamed(&[head, tail]), text.as_bytes(), "cut at {cut}");
        }
    }
}

#[cfg(feature = "cli")]
#[skuld::test]
fn an_escaped_character_split_across_writes_is_escaped_once() {
    let line_separator = "\u{2028}".as_bytes();
    for cut in 1..line_separator.len() {
        let (head, tail) = line_separator.split_at(cut);
        assert_eq!(streamed(&[head, tail]), br"\u{2028}", "cut at {cut}");
    }
    // One byte at a time, around the C1 control U+0085.
    assert_eq!(streamed(&[b"a", &[0xc2], &[0x85], b"b"]), br"a\u{85}b");
}

#[cfg(feature = "cli")]
#[skuld::test]
fn bytes_that_are_not_utf8_are_written_as_the_replacement_character() {
    assert_eq!(streamed(&[b"a\xffb\xc3"]), "a\u{fffd}b\u{fffd}".as_bytes());
    // A lead byte whose continuation never comes, then ASCII.
    assert_eq!(streamed(&[b"\xe2\x80", b"x"]), "\u{fffd}x".as_bytes());
}

#[cfg(feature = "cli")]
#[skuld::test]
fn an_incomplete_character_at_the_end_of_the_stream_is_not_lost_silently() {
    assert_eq!(streamed(&[b"ok\xe2\x80"]), "ok\u{fffd}".as_bytes());
}

#[cfg(feature = "cli")]
#[skuld::test]
fn a_stream_keeps_a_carriage_return_only_before_a_line_feed() {
    assert_eq!(streamed(&[b"a\r\nb"]), b"a\r\nb");
    assert_eq!(streamed(&[b"a\rb"]), br"a\u{d}b");
    assert_eq!(streamed(&[b"\r\r\n"]), concat!(r"\u{d}", "\r\n").as_bytes());
}

/// The CR at the end of one write waits for the next byte to learn which it is.
#[cfg(feature = "cli")]
#[skuld::test]
fn a_carriage_return_ending_a_write_is_decided_by_the_next_byte() {
    assert_eq!(streamed(&[b"a\r", b"\nb"]), b"a\r\nb");
    assert_eq!(streamed(&[b"a\r", b"b"]), br"a\u{d}b");
    assert_eq!(
        streamed(&[b"a\r", b"\r", b"\n"]),
        concat!("a", r"\u{d}", "\r\n").as_bytes()
    );
    // A write with nothing in it decides nothing.
    assert_eq!(streamed(&[b"a\r", b"", b"\n"]), b"a\r\n");
}

#[cfg(feature = "cli")]
#[skuld::test]
fn a_carriage_return_at_the_end_of_the_stream_is_escaped() {
    assert_eq!(streamed(&[b"ok\r"]), br"ok\u{d}");
}

/// What reached the inner stream, and how often it was flushed, readable while the stream is live.
#[cfg(feature = "cli")]
#[derive(Clone, Default)]
struct Probe(std::rc::Rc<std::cell::RefCell<(Vec<u8>, usize)>>);

#[cfg(feature = "cli")]
impl std::io::Write for Probe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().1 += 1;
        Ok(())
    }
}

/// A flush only flushes the inner stream: it does not say that no byte is due, so a tail held for
/// the next write stays held, and a CRLF split by a flush is still a CRLF. The drop writes it.
#[cfg(feature = "cli")]
#[skuld::test]
fn a_flush_keeps_a_held_tail_and_a_crlf_split_by_it_stays_whole() {
    let probe = Probe::default();
    let mut inner = probe.clone();
    {
        let mut stream = EscapedStream::new(&mut inner);
        std::io::Write::write_all(&mut stream, b"a\r").unwrap();
        std::io::Write::flush(&mut stream).unwrap();
        assert_eq!(probe.0.borrow().0, b"a", "the carriage return is still held");
        assert_eq!(probe.0.borrow().1, 1, "the inner stream is flushed");
        std::io::Write::write_all(&mut stream, b"\nb\xe2\x80").unwrap();
        std::io::Write::flush(&mut stream).unwrap();
        assert_eq!(
            probe.0.borrow().0,
            b"a\r\nb",
            "the CRLF is kept, half a character is held"
        );
        std::io::Write::write_all(&mut stream, b"\xa8\r").unwrap();
        std::io::Write::flush(&mut stream).unwrap();
        assert_eq!(
            probe.0.borrow().0,
            concat!("a\r\nb", r"\u{2028}").as_bytes(),
            "the last return is held again"
        );
    }
    assert_eq!(
        probe.0.borrow().0,
        concat!("a\r\nb", r"\u{2028}\u{d}").as_bytes(),
        "dropped, the held return is escaped"
    );
}

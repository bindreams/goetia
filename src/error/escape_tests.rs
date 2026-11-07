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

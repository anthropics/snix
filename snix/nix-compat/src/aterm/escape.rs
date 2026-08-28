use std::sync::LazyLock;

use aho_corasick::AhoCorasick;

const PATTERNS: [&str; 5] = ["\\", "\n", "\r", "\t", "\""];
const REPLACEMENTS: [&str; 5] = ["\\\\", "\\n", "\\r", "\\t", "\\\""];
static AC: LazyLock<AhoCorasick> = LazyLock::new(|| {
    AhoCorasick::builder()
        .build(PATTERNS)
        .expect("to init aho-corasick with PATTERNS")
});

/// Given a byte sequence, writes it in escaped form to the passed writer.
/// Does not add surrounding quotes.
pub fn write_escaped<P: AsRef<[u8]>>(s: P, w: &mut impl std::io::Write) -> std::io::Result<()> {
    let s = s.as_ref();
    let mut pos = 0;

    for m in AC.find_iter(s) {
        w.write_all(&s[pos..m.start()])?;
        w.write_all(REPLACEMENTS[m.pattern().as_usize()].as_bytes())?;
        pos = m.end();
    }

    w.write_all(&s[pos..])
}

#[cfg(test)]
mod tests {
    use super::write_escaped;
    use rstest::rstest;

    #[rstest]
    #[case::empty(b"", b"")]
    #[case::doublequote(b"\"", b"\\\"")]
    #[case::colon(b":", b":")]
    #[case::complex(b"foo\n\rbar\\baz", b"foo\\n\\rbar\\\\baz")]
    fn escape(#[case] input: &[u8], #[case] expected: &[u8]) {
        let mut buf = Vec::new();
        write_escaped(input, &mut buf).unwrap();

        assert_eq!(expected, buf.as_slice());
    }
}

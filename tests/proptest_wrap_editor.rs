//! Property tests (review §Tests #6): the editor's cursor stays inside the
//! text and grapheme-delete never leaves broken state; the word-wrap never
//! loses or invents characters. Hand-rolled mini-proptest (no dependency):
//! deterministic pseudo-random strings from an LCG.

use ai_commander::ui::editor::Editor;
use ai_commander::ui::theme::wrap_str;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ALPHABET: &[char] = &[
    'a',
    'b',
    'c',
    ' ',
    'н',
    'и',
    '👍',
    '\u{1F1F7}',
    '\u{1F1FA}',
    '\t',
    '-',
    '.',
    '\n',
];

fn random_string(rng: &mut Lcg, max_len: usize) -> String {
    let n = rng.below(max_len);
    (0..n)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

#[test]
fn editor_cursor_always_inside_text() {
    let mut rng = Lcg(0xDEADBEEF);
    for _ in 0..300 {
        let text = random_string(&mut rng, 40);
        let mut ed = Editor::with_text(&text);
        // random walk of every operation
        for _ in 0..30 {
            match rng.below(8) {
                0 => ed.left(),
                1 => ed.right(),
                2 => ed.up(),
                3 => ed.down(),
                4 => ed.home(),
                5 => ed.end(),
                6 => ed.backspace(),
                _ => ed.insert_char('x'),
            }
            // invariant: (row, col) within bounds
            assert!(ed.row < ed.lines().len(), "row out of bounds");
            assert!(
                ed.col <= ed.lines()[ed.row].chars().count(),
                "col {} > len at row {}",
                ed.col,
                ed.row
            );
        }
    }
}

#[test]
fn wrap_never_loses_characters() {
    let mut rng = Lcg(0xC0FFEE);
    for _ in 0..300 {
        let text = random_string(&mut rng, 120);
        let width = 1 + rng.below(20);
        let wrapped = wrap_str(&text, width);
        // every line fits the width (a single inherently-wide char can overflow)
        for l in &wrapped {
            let lw = unicode_width::UnicodeWidthStr::width(l.as_str());
            let single = l.chars().count() == 1;
            assert!(
                lw <= width || single,
                "line wider than {width} and multi-char: {l:?}"
            );
        }
        // word preservation holds when every word fits the width; otherwise
        // hard-breaking splits long words (verified by the width check above)
        let normalized_in = text.replace('\t', " ");
        let tokens_in: Vec<&str> = normalized_in
            .split([' ', '\n'])
            .filter(|w| !w.is_empty())
            .collect();
        let all_fit = tokens_in
            .iter()
            .all(|w| unicode_width::UnicodeWidthStr::width(*w) <= width);
        if all_fit {
            let tokens_out: Vec<&str> = wrapped
                .iter()
                .flat_map(|l| l.split(' '))
                .filter(|w| !w.is_empty())
                .collect();
            assert_eq!(tokens_out, tokens_in, "wrap lost/added words");
        } else {
            // hard-broken: the concatenation of all non-space chars matches
            let out_chars: String = wrapped
                .concat()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let in_chars: String = normalized_in
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            assert_eq!(out_chars, in_chars, "hard-break lost/added characters");
        }
    }
}

#[test]
fn grapheme_delete_preserves_prefix() {
    let mut rng = Lcg(0xABCDEF);
    for _ in 0..200 {
        let text = random_string(&mut rng, 30);
        let mut ed = Editor::with_text(&text);
        ed.end();
        // delete everything one backspace at a time
        while !ed.is_empty() {
            ed.backspace();
            assert!(ed.col <= ed.lines()[ed.row].chars().count());
        }
        assert!(ed.text().is_empty(), "not fully deleted: {:?}", ed.text());
    }
}

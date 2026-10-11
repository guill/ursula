//! Transcript assertion: normalized word-level Levenshtein distance ≤ 2 from 0.wav's text.

pub const REFERENCE: &str = "well i don't wish to see it any more observed phebe turning \
                             away her eyes it is certainly very like the old portrait";

/// Lowercase, keep only `[a-z0-9' ]`, collapse whitespace.
pub fn normalize(text: &str) -> String {
    let kept: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| matches!(c, 'a'..='z' | '0'..='9' | '\'' | ' '))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn word_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<_>, Vec<_>) = (a.split(' ').collect(), b.split(' ').collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, wa) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, wb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(wa != wb);
            row.push(sub.min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

pub fn assert_transcript(text: &str) {
    let got = normalize(text);
    let d = word_distance(&got, REFERENCE);
    assert!(d <= 2, "transcript distance {d} > 2: {got:?}");
}

#[test]
fn transcript_helpers() {
    assert_eq!(
        normalize(" Well,  I DON'T\nwish… 42! "),
        "well i don't wish 42"
    );
    assert_eq!(word_distance(REFERENCE, REFERENCE), 0);
    let one_off = REFERENCE.replace("phebe", "phoebe");
    assert_eq!(word_distance(&one_off, REFERENCE), 1);
    assert_eq!(word_distance("a b c", "a c"), 1);
    assert_eq!(word_distance("a b", "a b c d"), 2);
}

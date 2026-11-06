//! Text/ordering helpers that stand in for JavaScript string semantics.
//!
//! The TypeScript services order several catalogues with `String.prototype.localeCompare`
//! (sometimes with the explicit `"en"` locale). Rust's `Ord` on `str` is code-point order,
//! which differs for mixed-case names (`"B"` sorts before `"a"` in code points, after it in
//! ICU collation). These extension catalogues are shown to the user, so the difference is
//! observable in list order; [`locale_en_cmp`] reproduces the parts of ICU "en" ordering that
//! matter for file/dir names: case-insensitive primary weights, uppercase before lowercase as
//! the tie-break, then code points.
//!
//! Known, accepted divergence: ICU gives non-alphanumeric characters (punctuation, spaces)
//! their own primary weights ahead of letters. Here they compare by code point. That only
//! reorders names that differ solely in punctuation placement.

use std::cmp::Ordering;

/// Approximation of `left.localeCompare(right, "en")`.
pub fn locale_en_cmp(left: &str, right: &str) -> Ordering {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    for index in 0..std::cmp::min(left_chars.len(), right_chars.len()) {
        let left_char = left_chars[index];
        let right_char = right_chars[index];
        let left_folded = fold(left_char);
        let right_folded = fold(right_char);
        // ICU "en" compares numeric weights by value ("2" < "10"); comparing character by character would judge 2 to be greater.
        if left_char.is_ascii_digit() && right_char.is_ascii_digit() {
            let (left_run, left_len) = digits_run(&left_chars, index);
            let (right_run, right_len) = digits_run(&right_chars, index);
            if let (Ok(left_number), Ok(right_number)) =
                (left_run.parse::<u128>(), right_run.parse::<u128>())
            {
                if left_number != right_number {
                    return left_number.cmp(&right_number);
                }
                // Equal numeric value: consume the whole digit run and continue comparing from the next character.
                let consumed = left_len.max(right_len);
                let left_tail: String = left_chars[index + consumed..].iter().collect();
                let right_tail: String = right_chars[index + consumed..].iter().collect();
                return locale_en_cmp(&left_tail, &right_tail);
            }
        }
        match left_folded.cmp(&right_folded) {
            Ordering::Equal => {}
            other => return other,
        }
        // Same primary weight: ICU "en" orders uppercase before lowercase.
        if left_char.is_uppercase() != right_char.is_uppercase() {
            return if left_char.is_uppercase() {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
    }
    left_chars.len().cmp(&right_chars.len())
}

/// The run of consecutive ASCII digits starting at index (returns the string and its length).
fn digits_run(chars: &[char], index: usize) -> (String, usize) {
    let mut text = String::new();
    let mut length = 0;
    for character in chars[index..].iter() {
        if !character.is_ascii_digit() {
            break;
        }
        text.push(*character);
        length += 1;
    }
    (text, length.max(1))
}

fn fold(value: char) -> String {
    value.to_lowercase().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_with_uppercase_first() {
        // ICU "en": "Apple" < "apple" < "Banana"; code points would put "apple" after "Banana".
        assert_eq!(locale_en_cmp("Apple", "apple"), Ordering::Less);
        assert_eq!(locale_en_cmp("apple", "Banana"), Ordering::Less);
        assert_eq!(locale_en_cmp("banana", "Banana"), Ordering::Greater);
        assert_eq!(locale_en_cmp("notes", "notes"), Ordering::Equal);
        assert_eq!(locale_en_cmp("note", "notes"), Ordering::Less);
    }

    #[test]
    fn digits_sort_within_words_like_the_en_collator() {
        assert_eq!(locale_en_cmp("skill-2", "skill-10"), Ordering::Less);
    }
}

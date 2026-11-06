//! jsdiff `diff/base.js` A faithful port of Myers differential algorithm (row-level diff).
//!
//! Token semantics see [`crate::tokenize::tokenize`]: token = line with end-of-line break,
//! Equality is judged as strict string equality (jsdiff row-level path does not enable comparator/ignoreCase).
//! `options.oneChangePerToken` / `useLongestToken` / `postProcess` service only
//! For other diff variants such as word/json, the row-level path is always the default value and is not implemented here.
//!
//! Timeout semantic mirror jsdiff synchronization path: `while (editLength <= maxEditLength &&
//! Date.now() <= abortAfterTimestamp)`, millisecond truncation comparison; identity fast path is in
//! Return before timeout check (consistent with jsdiff).

use std::collections::HashMap;
use std::time::Instant;

use crate::tokenize::tokenize;

/// Difference fragment: component (including value) produced by jsdiff `buildValues`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    pub count: usize,
    pub added: bool,
    pub removed: bool,
    pub value: String,
    /// Line caching during patch build: engine output is always `None` (on demand `split_lines`),
    /// Only patch's sentinel fragment carries an explicit empty line array.
    pub lines: Option<Vec<String>>,
}

/// Edit a component node on the graph path (previous is the arena subscript, O(1) linked list).
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    previous: Option<usize>,
}

/// A path in bestPath: oldPos and the end of the linked list (jsdiff field with the same name).
#[derive(Debug, Clone, Copy)]
struct Path {
    old_pos: i64,
    last_component: Option<usize>,
}

/// The execution status of a diff calculation (old/new token and component arena).
struct Engine {
    old_tokens: Vec<String>,
    new_tokens: Vec<String>,
    arena: Vec<Component>,
}

impl Engine {
    fn push_component(
        &mut self,
        count: usize,
        added: bool,
        removed: bool,
        previous: Option<usize>,
    ) -> usize {
        self.arena.push(Component {
            count,
            added,
            removed,
            previous,
        });
        self.arena.len() - 1
    }

    /// jsdiff `extractCommon`: swallow the common token diagonally and return the new side position.
    fn extract_common(&mut self, path: &mut Path, diagonal: i64) -> i64 {
        let old_len = self.old_tokens.len() as i64;
        let new_len = self.new_tokens.len() as i64;
        let mut old_pos = path.old_pos;
        let mut new_pos = old_pos - diagonal;
        let mut common_count: usize = 0;
        while new_pos + 1 < new_len
            && old_pos + 1 < old_len
            && self.old_tokens[(old_pos + 1) as usize] == self.new_tokens[(new_pos + 1) as usize]
        {
            new_pos += 1;
            old_pos += 1;
            common_count += 1;
        }
        if common_count > 0 {
            let idx = self.push_component(common_count, false, false, path.last_component);
            path.last_component = Some(idx);
        }
        path.old_pos = old_pos;
        new_pos
    }

    /// jsdiff `addToPath`: attach an edit (add/remove) to the path,
    /// Merge count with tail similar edits (when oneChangePerToken is turned off).
    fn add_to_path(&mut self, path: Path, added: bool, removed: bool, old_pos_inc: i64) -> Path {
        let merged = path.last_component.is_some_and(|idx| {
            let last = &self.arena[idx];
            last.added == added && last.removed == removed
        });
        let previous = if merged {
            self.arena[path.last_component.expect("merged implies tail")].previous
        } else {
            path.last_component
        };
        let count = if merged {
            self.arena[path.last_component.expect("merged")].count + 1
        } else {
            1
        };
        let idx = self.push_component(count, added, removed, previous);
        Path {
            old_pos: path.old_pos + old_pos_inc,
            last_component: Some(idx),
        }
    }

    /// jsdiff `buildValues`: After backtracking along the linked list, the positive sequence is expanded into a sequence of fragments.
    fn build_values(&self, last: Option<usize>) -> Vec<Piece> {
        let mut chain: Vec<usize> = Vec::new();
        let mut cursor = last;
        while let Some(idx) = cursor {
            chain.push(idx);
            cursor = self.arena[idx].previous;
        }
        chain.reverse();

        let mut pieces = Vec::with_capacity(chain.len());
        let mut new_pos: usize = 0;
        let mut old_pos: usize = 0;
        for &idx in &chain {
            let component = &self.arena[idx];
            // useLongestToken is always false: non-removed fragments take the new side token.
            let (source, start) = if component.removed {
                (&self.old_tokens, old_pos)
            } else {
                (&self.new_tokens, new_pos)
            };
            // jsdiff Array#slice semantics: out-of-bounds endpoints are clamped to the array length.
            let begin = start.min(source.len());
            let end = (start + component.count).min(source.len());
            let value = source[begin..end].concat();
            if component.removed {
                old_pos = start + component.count;
            } else {
                new_pos = start + component.count;
                if !component.added {
                    old_pos = start + component.count;
                }
            }
            pieces.push(Piece {
                count: component.count,
                added: component.added,
                removed: component.removed,
                value,
                lines: None,
            });
        }
        pieces
    }

    /// jsdiff `execEditLength`: Advance a round of diagonal scan with a fixed number of edits.
    /// Returns `Some` to indicate that the end point has been reached (even if the fragment is an empty array).
    fn exec_edit_length(
        &mut self,
        edit_length: i64,
        best_path: &mut HashMap<i64, Path>,
        min_diagonal: &mut i64,
        max_diagonal: &mut i64,
    ) -> Option<Vec<Piece>> {
        let old_len = self.old_tokens.len() as i64;
        let new_len = self.new_tokens.len() as i64;
        // The ±Infinity boundary of jsdiff will never take effect in the first round, and i64 extreme values ​​are semantically equivalent.
        let mut diagonal = (*min_diagonal).max(-edit_length);
        let end = (*max_diagonal).min(edit_length);
        while diagonal <= end {
            // jsdiff: Take out diag-1 as a remove candidate and clear it from bestPath smoothly.
            let remove_path = best_path.remove(&(diagonal - 1));
            let add_path = best_path.get(&(diagonal + 1)).copied();
            let can_add = add_path.is_some_and(|path| {
                let new_pos = path.old_pos - diagonal;
                0 <= new_pos && new_pos < new_len
            });
            let can_remove = remove_path.is_some_and(|path| path.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                // Terminal path pruning (jsdiff clears the diag slot synchronously).
                best_path.remove(&diagonal);
                diagonal += 2;
                continue;
            }
            // Select the branch further away from oldPos; if remove is not possible, add must be used (otherwise it has been pruned).
            let take_add = match (can_remove, can_add) {
                (false, _) => true,
                (true, false) => false,
                (true, true) => {
                    let remove = remove_path.expect("can_remove implies path");
                    let add = add_path.expect("can_add implies path");
                    remove.old_pos < add.old_pos
                }
            };
            let mut base_path = if take_add {
                self.add_to_path(add_path.expect("take_add implies path"), true, false, 0)
            } else {
                self.add_to_path(remove_path.expect("remove branch"), false, true, 1)
            };
            let new_pos = self.extract_common(&mut base_path, diagonal);
            if base_path.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return Some(self.build_values(base_path.last_component));
            }
            best_path.insert(diagonal, base_path);
            // jsdiff edge pruning: no longer expands the diagonal line in the corresponding direction after touching bottom/right.
            if base_path.old_pos + 1 >= old_len {
                *max_diagonal = (*max_diagonal).min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                *min_diagonal = (*min_diagonal).max(diagonal + 1);
            }
            diagonal += 2;
        }
        None
    }

    /// Main loop: identity fast path → expand the number of edits step by step until completed/overrun/timeout.
    fn run(&mut self, timeout_ms: u128) -> Option<Vec<Piece>> {
        let old_len = self.old_tokens.len() as i64;
        let new_len = self.new_tokens.len() as i64;
        let mut best_path: HashMap<i64, Path> = HashMap::new();
        best_path.insert(
            0,
            Path {
                old_pos: -1,
                last_component: None,
            },
        );
        let mut seed = best_path[&0];
        let new_pos = self.extract_common(&mut seed, 0);
        best_path.insert(0, seed);
        if seed.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
            // identity: Returns before timeout check like jsdiff.
            return Some(self.build_values(seed.last_component));
        }

        let mut edit_length: i64 = 1;
        let max_edit_length = old_len + new_len;
        let mut min_diagonal = i64::MIN;
        let mut max_diagonal = i64::MAX;
        let start = Instant::now();
        while edit_length <= max_edit_length && start.elapsed().as_millis() <= timeout_ms {
            if let Some(pieces) = self.exec_edit_length(
                edit_length,
                &mut best_path,
                &mut min_diagonal,
                &mut max_diagonal,
            ) {
                return Some(pieces);
            }
            edit_length += 1;
        }
        // Timeout or the number of edits reaches the top: jsdiff returns undefined and maps to None.
        None
    }
}

/// Row-level difference entry: Return `None` to indicate timeout/the number of edits has reached the top (undefined in jsdiff).
pub(crate) fn diff_pieces(old: &str, new: &str, timeout_ms: u128) -> Option<Vec<Piece>> {
    let mut engine = Engine {
        old_tokens: tokenize(old),
        new_tokens: tokenize(new),
        arena: Vec::new(),
    };
    engine.run(timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(old: &str, new: &str) -> Vec<(usize, bool, bool, String)> {
        diff_pieces(old, new, u128::MAX)
            .expect("Infinite timeout must produce results")
            .into_iter()
            .map(|piece| (piece.count, piece.added, piece.removed, piece.value))
            .collect()
    }

    #[test]
    fn identical_inputs_yield_single_common_piece() {
        assert_eq!(
            values("a\nb\n", "a\nb\n"),
            vec![(2, false, false, "a\nb\n".to_string())]
        );
        assert!(values("", "").is_empty(), "Empty↔Empty: no fragment");
    }

    #[test]
    fn insert_delete_replace_shape_matches_components() {
        // Insert: public + new.
        let pieces = values("a\n", "a\nb\n");
        assert_eq!(pieces.len(), 2);
        assert!(!pieces[0].1 && !pieces[0].2);
        assert_eq!(
            (pieces[1].1, pieces[1].2, pieces[1].3.as_str()),
            (true, false, "b\n")
        );
        // Delete: public + delete.
        let pieces = values("a\nb\n", "a\n");
        assert_eq!(pieces.len(), 2);
        assert_eq!(
            (pieces[1].1, pieces[1].2, pieces[1].3.as_str()),
            (false, true, "b\n")
        );
        // Replacement: delete + add two paragraphs.
        let pieces = values("a\nb\n", "a\nB\n");
        assert_eq!(pieces.len(), 3);
        assert!(pieces[1].2 && pieces[1].3 == "b\n");
        assert!(pieces[2].1 && pieces[2].3 == "B\n");
    }

    #[test]
    fn crlf_lines_compare_with_carriage_return_included() {
        // `\r` Attribute line content: CRLF and LF are not equal → a difference must be made.
        let pieces = values("a\r\nb\n", "a\nb\n");
        assert!(pieces.iter().any(|piece| piece.1 || piece.2), "{pieces:?}");
        // Both sides are CRLF → exactly equal.
        assert_eq!(values("a\r\nb\r\n", "a\r\nb\r\n").len(), 1);
    }

    #[test]
    fn timeout_zero_aborts_but_identity_fast_path_survives() {
        // Completely disjoint large inputs: number of edits cannot top out in 0ms → timeout None (jsdiff undefined).
        let old: String = (0..5000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("new {i}\n")).collect();
        assert!(diff_pieces(&old, &new, 0).is_none());

        // identity fast path returns before timeout check → results even with timeout 0 (mirror jsdiff).
        assert!(diff_pieces("same\n", "same\n", 0).is_some());
    }

    #[test]
    fn disjoint_inputs_produce_paired_removal_then_insertion() {
        let pieces = values("x\n", "y\n");
        //Edit distance 2: one delete and one add (the order is determined by branch selection, both are required).
        assert_eq!(pieces.len(), 2);
        assert_ne!(
            (pieces[0].1, pieces[0].2),
            (pieces[1].1, pieces[1].2),
            "Two paragraphs must be added and subtracted"
        );
    }
}

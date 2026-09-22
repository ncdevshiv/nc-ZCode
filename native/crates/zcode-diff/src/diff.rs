//! jsdiff `diff/base.js` Myers 差分算法的忠实移植（行级 diff）。
//!
//! token 语义见 [`crate::tokenize::tokenize`]：token = 带行尾换行的行，
//! 相等判断为严格字符串相等（jsdiff 行级路径未开启 comparator/ignoreCase）。
//! `options.oneChangePerToken` / `useLongestToken` / `postProcess` 仅服务
//! word/json 等其它 diff 变体，行级路径恒为默认值，此处不实现。
//!
//! 超时语义镜像 jsdiff 同步路径：`while (editLength <= maxEditLength &&
//! Date.now() <= abortAfterTimestamp)`，毫秒截断比较；identity 快路径在
//! 超时检查之前返回（与 jsdiff 一致）。

use std::collections::HashMap;
use std::time::Instant;

use crate::tokenize::tokenize;

/// 差分片段：jsdiff `buildValues` 产出的 component（含 value）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    pub count: usize,
    pub added: bool,
    pub removed: bool,
    pub value: String,
    /// patch 构建时的行缓存：引擎产出恒为 `None`（按需 `split_lines`），
    /// 仅 patch 的哨兵片段携带显式空行数组。
    pub lines: Option<Vec<String>>,
}

/// 编辑图路径上的一个 component 节点（previous 为 arena 下标，O(1) 链表）。
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    previous: Option<usize>,
}

/// bestPath 中的一条路径：oldPos 与链表尾（jsdiff 同名字段）。
#[derive(Debug, Clone, Copy)]
struct Path {
    old_pos: i64,
    last_component: Option<usize>,
}

/// 一次 diff 计算的执行期状态（old/new token 与 component arena）。
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

    /// jsdiff `extractCommon`：沿对角线吞掉公共 token，返回 new 侧位置。
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

    /// jsdiff `addToPath`：把一次编辑（add/remove）挂到路径上，
    /// 与尾部同类编辑合并计数（oneChangePerToken 关闭时）。
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

    /// jsdiff `buildValues`：沿链表回溯后正序展开为片段序列。
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
            // useLongestToken 恒 false：非 removed 片段取 new 侧 token。
            let (source, start) = if component.removed {
                (&self.old_tokens, old_pos)
            } else {
                (&self.new_tokens, new_pos)
            };
            // jsdiff Array#slice 语义：越界端点钳制到数组长度。
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

    /// jsdiff `execEditLength`：推进一轮固定编辑数的对角线扫描。
    /// 返回 `Some` 表示已抵达终点（即使片段为空数组也算完成）。
    fn exec_edit_length(
        &mut self,
        edit_length: i64,
        best_path: &mut HashMap<i64, Path>,
        min_diagonal: &mut i64,
        max_diagonal: &mut i64,
    ) -> Option<Vec<Piece>> {
        let old_len = self.old_tokens.len() as i64;
        let new_len = self.new_tokens.len() as i64;
        // jsdiff 的 ±Infinity 边界在首轮恒不生效，i64 极值语义等价。
        let mut diagonal = (*min_diagonal).max(-edit_length);
        let end = (*max_diagonal).min(edit_length);
        while diagonal <= end {
            // jsdiff：取出 diag-1 作为 remove 候选并顺手从 bestPath 清掉。
            let remove_path = best_path.remove(&(diagonal - 1));
            let add_path = best_path.get(&(diagonal + 1)).copied();
            let can_add = add_path.is_some_and(|path| {
                let new_pos = path.old_pos - diagonal;
                0 <= new_pos && new_pos < new_len
            });
            let can_remove = remove_path.is_some_and(|path| path.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                // 终端路径剪枝（jsdiff 同步清掉 diag 槽位）。
                best_path.remove(&diagonal);
                diagonal += 2;
                continue;
            }
            // 选择 oldPos 更远的分支；不可 remove 时必走 add（否则已剪枝）。
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
            // jsdiff 边缘剪枝：触底/触右后不再扩展对应方向的对角线。
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

    /// 主循环：identity 快路径 → 逐级扩编辑数，直到完成/超限/超时。
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
            // identity：与 jsdiff 一样在超时检查之前返回。
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
        // 超时或编辑数触顶：jsdiff 返回 undefined，映射为 None。
        None
    }
}

/// 行级差分入口：返回 `None` 表示超时/编辑数触顶（jsdiff 的 undefined）。
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
            .expect("无限超时必须产出结果")
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
        assert!(values("", "").is_empty(), "空↔空：无片段");
    }

    #[test]
    fn insert_delete_replace_shape_matches_components() {
        // 插入：公共 + 新增。
        let pieces = values("a\n", "a\nb\n");
        assert_eq!(pieces.len(), 2);
        assert!(!pieces[0].1 && !pieces[0].2);
        assert_eq!(
            (pieces[1].1, pieces[1].2, pieces[1].3.as_str()),
            (true, false, "b\n")
        );
        // 删除：公共 + 删除。
        let pieces = values("a\nb\n", "a\n");
        assert_eq!(pieces.len(), 2);
        assert_eq!(
            (pieces[1].1, pieces[1].2, pieces[1].3.as_str()),
            (false, true, "b\n")
        );
        // 替换：删 + 增两段。
        let pieces = values("a\nb\n", "a\nB\n");
        assert_eq!(pieces.len(), 3);
        assert!(pieces[1].2 && pieces[1].3 == "b\n");
        assert!(pieces[2].1 && pieces[2].3 == "B\n");
    }

    #[test]
    fn crlf_lines_compare_with_carriage_return_included() {
        // `\r` 属行内容：CRLF 与 LF 不相等 → 必须产生差异。
        let pieces = values("a\r\nb\n", "a\nb\n");
        assert!(pieces.iter().any(|piece| piece.1 || piece.2), "{pieces:?}");
        // 两侧同为 CRLF → 完全相等。
        assert_eq!(values("a\r\nb\r\n", "a\r\nb\r\n").len(), 1);
    }

    #[test]
    fn timeout_zero_aborts_but_identity_fast_path_survives() {
        // 完全不相交的大输入：编辑数无法在 0ms 内触顶 → 超时 None（jsdiff undefined）。
        let old: String = (0..5000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("new {i}\n")).collect();
        assert!(diff_pieces(&old, &new, 0).is_none());

        // identity 快路径先于超时检查返回 → 即使 timeout 0 也有结果（镜像 jsdiff）。
        assert!(diff_pieces("same\n", "same\n", 0).is_some());
    }

    #[test]
    fn disjoint_inputs_produce_paired_removal_then_insertion() {
        let pieces = values("x\n", "y\n");
        // 编辑距离 2：一删一增（顺序由分支选择决定，两者齐备即可）。
        assert_eq!(pieces.len(), 2);
        assert_ne!(
            (pieces[0].1, pieces[0].2),
            (pieces[1].1, pieces[1].2),
            "两段必须一增一减"
        );
    }
}

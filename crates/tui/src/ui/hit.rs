//! マウスヒット判定用の小さなテーブル。
//!
//! タイムラインは可変高さの note を縦に並べる。描画時に「note index `i` は
//! `top` 行から `height` 行を占有する」を [`ScrollHits`] に push しておき、
//! クリック座標 (`row`) を受け取って線形探索で index を返す。
//! 件数は高々 1 page (= 80 件) なので overkill な探索木は不要。

use ratatui::layout::Rect;

#[derive(Debug, Default, Clone, Copy)]
pub struct Hit {
    pub note_index: usize,
    pub top: u16,
    pub height: u16,
}

#[derive(Debug, Default, Clone)]
pub struct ScrollHits {
    rows: Vec<Hit>,
}

impl ScrollHits {
    pub fn push(&mut self, note_index: usize, top: u16, height: u16) {
        if height == 0 {
            return;
        }
        self.rows.push(Hit {
            note_index,
            top,
            height,
        });
    }

    /// クリックされた row が乗っている note index を返す。
    pub fn resolve(&self, row: u16) -> Option<usize> {
        for hit in &self.rows {
            if row >= hit.top && row < hit.top + hit.height {
                return Some(hit.note_index);
            }
        }
        None
    }

    pub fn contains_in_rect(&self, rect: Rect, row: u16, col: u16) -> bool {
        col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// 固定高さの一覧画面 (follow list / follow requests / notifications /
/// lists) 共通のクリック解決。各エントリが `row_step` 行 (avatar 有 = 2、
/// 無 = 1) を占め、`top` 件目からスクロール表示している前提。`rect` の外の
/// クリックや、一覧の実件数 (`len`) を超える位置へのクリックは `None`。
///
/// [`ScrollHits`] と役割は同じだが、こちらは可変高さ note を線形探索する
/// 必要が無い (= 描画時に行位置を積む手間を省ける) 一覧画面向けの軽量版。
#[must_use]
pub fn resolve_fixed_row(
    rect: Rect,
    row: u16,
    row_step: u16,
    top: usize,
    len: usize,
) -> Option<usize> {
    if row < rect.y || row >= rect.y + rect.height || row_step == 0 {
        return None;
    }
    let offset = usize::from((row - rect.y) / row_step);
    let idx = top + offset;
    (idx < len).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_finds_correct_index() {
        let mut hits = ScrollHits::default();
        hits.push(0, 1, 3);
        hits.push(1, 4, 2);
        hits.push(2, 6, 4);
        assert_eq!(hits.resolve(0), None);
        assert_eq!(hits.resolve(1), Some(0));
        assert_eq!(hits.resolve(3), Some(0));
        assert_eq!(hits.resolve(4), Some(1));
        assert_eq!(hits.resolve(5), Some(1));
        assert_eq!(hits.resolve(6), Some(2));
        assert_eq!(hits.resolve(9), Some(2));
        assert_eq!(hits.resolve(10), None);
    }

    #[test]
    fn height_zero_skipped() {
        let mut hits = ScrollHits::default();
        hits.push(0, 5, 0);
        assert!(hits.is_empty());
        assert_eq!(hits.resolve(5), None);
    }

    #[test]
    fn rect_containment_works() {
        let hits = ScrollHits::default();
        let r = Rect::new(0, 0, 10, 5);
        assert!(hits.contains_in_rect(r, 0, 0));
        assert!(hits.contains_in_rect(r, 4, 9));
        assert!(!hits.contains_in_rect(r, 5, 0));
        assert!(!hits.contains_in_rect(r, 0, 10));
    }

    #[test]
    fn resolve_fixed_row_single_step() {
        let rect = Rect::new(0, 2, 10, 5); // rows 2..=6
        assert_eq!(resolve_fixed_row(rect, 2, 1, 0, 5), Some(0));
        assert_eq!(resolve_fixed_row(rect, 4, 1, 0, 5), Some(2));
        assert_eq!(resolve_fixed_row(rect, 6, 1, 0, 5), Some(4));
    }

    #[test]
    fn resolve_fixed_row_respects_top_offset() {
        let rect = Rect::new(0, 0, 10, 5);
        // 一覧の 3 件目からスクロール表示中。
        assert_eq!(resolve_fixed_row(rect, 0, 1, 3, 10), Some(3));
        assert_eq!(resolve_fixed_row(rect, 2, 1, 3, 10), Some(5));
    }

    #[test]
    fn resolve_fixed_row_two_row_step_for_avatar() {
        let rect = Rect::new(0, 0, 10, 6);
        assert_eq!(resolve_fixed_row(rect, 0, 2, 0, 3), Some(0));
        assert_eq!(resolve_fixed_row(rect, 1, 2, 0, 3), Some(0));
        assert_eq!(resolve_fixed_row(rect, 2, 2, 0, 3), Some(1));
        assert_eq!(resolve_fixed_row(rect, 4, 2, 0, 3), Some(2));
    }

    #[test]
    fn resolve_fixed_row_out_of_rect_or_past_len_is_none() {
        let rect = Rect::new(0, 5, 10, 3); // rows 5..=7
        assert_eq!(resolve_fixed_row(rect, 4, 1, 0, 10), None, "above rect");
        assert_eq!(resolve_fixed_row(rect, 8, 1, 0, 10), None, "below rect");
        assert_eq!(
            resolve_fixed_row(rect, 6, 1, 0, 1),
            None,
            "index beyond len"
        );
        assert_eq!(resolve_fixed_row(rect, 6, 0, 0, 10), None, "zero row_step");
    }
}

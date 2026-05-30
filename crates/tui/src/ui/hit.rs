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
}

use std::collections::HashMap;

/// Reciprocal rank fusion: each list adds 1 / (k + rank) for every id it holds.
pub fn rrf(lists: &[Vec<i64>], k: f64) -> HashMap<i64, f64> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_in_two_lists_beats_item_in_one() {
        let s = rrf(&[vec![1, 2], vec![2, 3]], 60.0);
        assert!(s[&2] > s[&1]);
        assert!(s[&1] > s[&3]);
    }

    #[test]
    fn empty_lists_give_empty_scores() {
        assert!(rrf(&[], 60.0).is_empty());
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct IntervalSet {
    spans: Vec<(f64, f64)>,
}

impl IntervalSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_spans(mut spans: Vec<(f64, f64)>) -> Self {
        spans.retain(|&(lo, hi)| hi > lo);
        spans.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(spans.len());
        for (lo, hi) in spans {
            match merged.last_mut() {
                Some((_, end)) if lo <= *end => *end = end.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        Self { spans: merged }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn spans(&self) -> &[(f64, f64)] {
        &self.spans
    }

    pub fn clear(&mut self) {
        self.spans.clear();
    }

    pub fn measure(&self) -> f64 {
        self.spans.iter().map(|(lo, hi)| hi - lo).sum()
    }

    pub fn union_with(&mut self, other: &Self) {
        for &(lo, hi) in &other.spans {
            self.insert_span(lo, hi);
        }
    }

    fn insert_span(&mut self, mut lo: f64, mut hi: f64) {
        if hi <= lo {
            return;
        }
        let mut start = 0;
        while start < self.spans.len() && self.spans[start].1 < lo {
            start += 1;
        }
        let mut end = start;
        while end < self.spans.len() && self.spans[end].0 <= hi {
            lo = lo.min(self.spans[end].0);
            hi = hi.max(self.spans[end].1);
            end += 1;
        }
        if start == end {
            self.spans.insert(start, (lo, hi));
        } else {
            self.spans[start] = (lo, hi);
            if start + 1 < end {
                self.spans.drain(start + 1..end);
            }
        }
    }

    pub fn difference(&self, other: &Self) -> Self {
        let mut spans = Vec::new();
        for &(lo, hi) in &self.spans {
            let mut cursor = lo;
            let start = other.spans.partition_point(|&(_, end)| end <= cursor);
            for &(o_lo, o_hi) in &other.spans[start..] {
                if o_lo >= hi {
                    break;
                }
                if o_lo > cursor {
                    spans.push((cursor, o_lo));
                }
                cursor = cursor.max(o_hi);
                if cursor >= hi {
                    break;
                }
            }
            if cursor < hi {
                spans.push((cursor, hi));
            }
        }
        Self { spans }
    }

    pub fn union_measure(&self, other: &Self) -> f64 {
        if self.spans.is_empty() {
            return other.measure();
        }
        if other.spans.is_empty() {
            return self.measure();
        }
        let (mut a, mut b) = (0, 0);
        let mut total = 0.0;
        let mut run: Option<(f64, f64)> = None;
        while a < self.spans.len() || b < other.spans.len() {
            let next = if b == other.spans.len()
                || (a < self.spans.len() && self.spans[a].0 <= other.spans[b].0)
            {
                let span = self.spans[a];
                a += 1;
                span
            } else {
                let span = other.spans[b];
                b += 1;
                span
            };
            run = match run {
                Some((lo, hi)) if next.0 <= hi => Some((lo, hi.max(next.1))),
                Some((lo, hi)) => {
                    total += hi - lo;
                    Some(next)
                }
                None => Some(next),
            };
        }
        if let Some((lo, hi)) = run {
            total += hi - lo;
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_spans_sorts_merges_and_drops_empties() {
        let s = IntervalSet::from_spans(vec![(5.0, 7.0), (1.0, 2.0), (2.0, 3.0), (6.0, 6.0)]);
        assert_eq!(s.spans(), &[(1.0, 3.0), (5.0, 7.0)]);
        assert_eq!(s.measure(), 4.0);
    }

    #[test]
    fn overlapping_spans_merge() {
        let s = IntervalSet::from_spans(vec![(0.0, 2.0), (1.0, 5.0), (4.0, 6.0)]);
        assert_eq!(s.spans(), &[(0.0, 6.0)]);
    }

    #[test]
    fn union_with_matches_union_measure() {
        let a = IntervalSet::from_spans(vec![(0.0, 2.0), (4.0, 6.0), (10.0, 11.0)]);
        let b = IntervalSet::from_spans(vec![(1.0, 5.0), (8.0, 9.0)]);
        let expected = a.union_measure(&b);
        let mut u = a.clone();
        u.union_with(&b);
        assert_eq!(u.measure(), expected);
        assert_eq!(u.spans(), &[(0.0, 6.0), (8.0, 9.0), (10.0, 11.0)]);
        assert_eq!(a.union_measure(&IntervalSet::new()), a.measure());
    }

    #[test]
    fn union_with_inserts_before_after_and_inside() {
        let mut s = IntervalSet::from_spans(vec![(4.0, 5.0), (7.0, 8.0)]);
        s.union_with(&IntervalSet::from_spans(vec![
            (1.0, 2.0),
            (5.0, 7.0),
            (9.0, 10.0),
        ]));
        assert_eq!(s.spans(), &[(1.0, 2.0), (4.0, 8.0), (9.0, 10.0)]);
    }

    #[test]
    fn difference_clips_overlaps_and_keeps_the_rest() {
        let a = IntervalSet::from_spans(vec![(0.0, 10.0), (20.0, 30.0)]);
        let b = IntervalSet::from_spans(vec![(-5.0, 2.0), (4.0, 6.0), (12.0, 15.0)]);
        assert_eq!(
            a.difference(&b).spans(),
            &[(2.0, 4.0), (6.0, 10.0), (20.0, 30.0)]
        );
        let covered = IntervalSet::from_spans(vec![(0.0, 40.0)]);
        assert!(a.difference(&covered).is_empty());
        assert_eq!(a.difference(&IntervalSet::new()), a);
        assert!(IntervalSet::new().difference(&b).is_empty());
    }

    #[test]
    fn difference_measure_matches_union_minus_other() {
        let cases = [
            (vec![(0.0, 10.0)], vec![(3.0, 4.0), (5.0, 20.0)]),
            (vec![(0.0, 1.0), (2.0, 3.0)], vec![(0.5, 2.5)]),
            (vec![(0.0, 5.0)], vec![(5.0, 9.0)]),
            (vec![(1.0, 2.0)], vec![(1.0, 2.0)]),
        ];
        for (left, right) in cases {
            let a = IntervalSet::from_spans(left);
            let b = IntervalSet::from_spans(right);
            let expected = a.union_measure(&b) - b.measure();
            assert!(
                (a.difference(&b).measure() - expected).abs() < 1e-12,
                "{:?} minus {:?}",
                a.spans(),
                b.spans()
            );
        }
    }

    #[test]
    fn empty_set() {
        let s = IntervalSet::new();
        assert!(s.is_empty());
        assert_eq!(s.measure(), 0.0);
        assert_eq!(s.union_measure(&s), 0.0);
    }
}

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::Path;

pub struct Candidate<'a> {
    pub idx: usize,
    pub path: &'a Path,
    pub bytes: usize,
    pub is_estimate: bool,
    pub is_jump: bool,
}

#[derive(Default)]
pub struct Prefetch {
    pub indices: Vec<usize>,
    pub probes: Vec<usize>,
}

pub fn select<'a>(
    current: &'a Path,
    ahead: impl Iterator<Item = Candidate<'a>>,
    behind: impl Iterator<Item = Candidate<'a>>,
    budget: usize,
    bias: NonZeroUsize,
) -> Prefetch {
    if budget == 0 {
        return Prefetch::default();
    }

    let mut ahead = ahead.peekable();
    let mut behind = behind.peekable();
    let mut ahead_indices = Vec::new();
    let mut behind_indices = Vec::new();
    let mut seen = HashSet::from([current]);

    let mut behind_bytes = bias.get().checked_add(1).map_or(0, |ratio| budget / ratio);
    let mut ahead_bytes = budget - behind_bytes;
    fill(
        &mut ahead,
        &mut ahead_indices,
        &mut seen,
        &mut ahead_bytes,
        budget,
    );
    fill(
        &mut behind,
        &mut behind_indices,
        &mut seen,
        &mut behind_bytes,
        budget,
    );

    let mut spare = ahead_bytes + behind_bytes;
    fill(
        &mut ahead,
        &mut ahead_indices,
        &mut seen,
        &mut spare,
        budget,
    );
    let jump = if spare > 0 && ahead.peek().is_some_and(|candidate| candidate.is_jump) {
        // A jump that cannot fit should not block the nearer images.
        let jump = ahead.next();
        fill(
            &mut ahead,
            &mut ahead_indices,
            &mut seen,
            &mut spare,
            budget,
        );
        jump
    } else {
        None
    };
    fill(
        &mut behind,
        &mut behind_indices,
        &mut seen,
        &mut spare,
        budget,
    );

    // The viewport is an upper bound for an unknown image. Try the next image
    // on either side in case it fits. Only admit it once we have its size.
    let mut probes = Vec::new();
    if spare > 0 {
        let ahead = ahead
            .next()
            .filter(|candidate| candidate.is_estimate)
            .or(jump);
        for candidate in [ahead, behind.next()].into_iter().flatten() {
            if candidate.is_estimate && seen.insert(candidate.path) {
                probes.push(candidate.idx);
            }
        }
    }

    ahead_indices.extend(behind_indices);
    Prefetch {
        indices: ahead_indices,
        probes,
    }
}

fn fill<'a>(
    candidates: &mut std::iter::Peekable<impl Iterator<Item = Candidate<'a>>>,
    selected: &mut Vec<usize>,
    seen: &mut HashSet<&'a Path>,
    left: &mut usize,
    budget: usize,
) {
    while *left > 0 {
        let Some(candidate) = candidates.peek() else {
            return;
        };
        if seen.contains(candidate.path) || (!candidate.is_estimate && candidate.bytes > budget) {
            candidates.next();
            continue;
        }
        let Some(after) = left.checked_sub(candidate.bytes) else {
            return;
        };
        *left = after;
        seen.insert(candidate.path);
        selected.push(candidate.idx);
        candidates.next();
    }
}

pub fn indices(
    cur: usize,
    len: usize,
    step: isize,
    repeat_jump: bool,
    ahead: bool,
) -> impl Iterator<Item = usize> {
    let ascending = (step >= 0) == ahead;
    let max_offset = if ascending {
        len.saturating_sub(cur + 1)
    } else {
        cur
    };
    let jump = cur
        .checked_add_signed(step)
        .filter(|&idx| ahead && repeat_jump && step.unsigned_abs() > 1 && idx < len);
    jump.into_iter().chain(
        (1..=max_offset)
            .map(move |offset| {
                if ascending {
                    cur + offset
                } else {
                    cur - offset
                }
            })
            .filter(move |&idx| Some(idx) != jump),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(cur: usize, len: usize, step: isize, budget: usize, bias: usize) -> Prefetch {
        let paths: Vec<_> = (0..len)
            .map(|idx| std::path::PathBuf::from(idx.to_string()))
            .collect();
        let candidates = |ahead| {
            indices(cur, len, step, true, ahead).map(|idx| Candidate {
                idx,
                path: paths[idx].as_path(),
                bytes: 128,
                is_estimate: false,
                is_jump: idx == cur.saturating_add_signed(step) && step.unsigned_abs() > 1,
            })
        };
        select(
            &paths[cur],
            candidates(true),
            candidates(false),
            budget,
            NonZeroUsize::new(bias).unwrap(),
        )
    }

    #[test]
    fn test_bias_follows_direction() {
        assert_eq!(plan(5, 11, 1, 512, 3).indices, [6, 7, 8, 4]);
        assert_eq!(plan(5, 11, -1, 512, 3).indices, [4, 3, 2, 6]);
        assert_eq!(plan(5, 11, 1, 512, 1).indices, [6, 7, 4, 3]);
    }

    #[test]
    fn test_lends_spare_at_list_ends() {
        assert_eq!(plan(0, 5, 1, 512, 3).indices, [1, 2, 3, 4]);
        assert_eq!(plan(4, 5, 1, 512, 3).indices, [3, 2, 1, 0]);
    }

    #[test]
    fn test_repeat_jump_first_without_duplicates() {
        assert_eq!(plan(5, 11, 2, 512, 3).indices, [7, 6, 8, 4]);
        assert_eq!(plan(5, 11, -3, 512, 3).indices, [2, 4, 3, 6]);
        assert_eq!(plan(9, 11, 5, 512, 3).indices, [10, 8, 7, 6]);
    }

    #[test]
    fn test_jump_that_cannot_fit_leaves_room_for_neighbours() {
        let ahead = [
            Candidate {
                idx: 5,
                path: Path::new("jump"),
                bytes: 80,
                is_estimate: false,
                is_jump: true,
            },
            Candidate {
                idx: 1,
                path: Path::new("next"),
                bytes: 25,
                is_estimate: false,
                is_jump: false,
            },
        ];
        let behind = [Candidate {
            idx: 2,
            path: Path::new("previous"),
            bytes: 25,
            is_estimate: false,
            is_jump: false,
        }];
        let plan = select(
            Path::new("current"),
            ahead.into_iter(),
            behind.into_iter(),
            100,
            NonZeroUsize::new(3).unwrap(),
        );
        assert_eq!(plan.indices, [1, 2]);
    }

    #[test]
    fn test_probe_is_separate_from_admission() {
        let current = Path::new("current");
        let ahead = [
            Candidate {
                idx: 1,
                path: Path::new("known"),
                bytes: 30,
                is_estimate: false,
                is_jump: false,
            },
            Candidate {
                idx: 2,
                path: Path::new("too large"),
                bytes: 120,
                is_estimate: false,
                is_jump: false,
            },
            Candidate {
                idx: 3,
                path: Path::new("unknown"),
                bytes: 120,
                is_estimate: true,
                is_jump: false,
            },
        ];
        let behind = [Candidate {
            idx: 4,
            path: Path::new("behind"),
            bytes: 25,
            is_estimate: false,
            is_jump: false,
        }];
        let plan = select(
            current,
            ahead.into_iter(),
            behind.into_iter(),
            55 + 1,
            NonZeroUsize::MIN,
        );
        assert_eq!(plan.indices, [1, 4]);
        assert_eq!(plan.probes, [3]);
    }

    #[test]
    fn test_zero_budget_never_probes() {
        let ahead = [Candidate {
            idx: 1,
            path: Path::new("unknown"),
            bytes: 120,
            is_estimate: true,
            is_jump: false,
        }];
        let plan = select(
            Path::new("current"),
            ahead.into_iter(),
            std::iter::empty(),
            0,
            NonZeroUsize::MIN,
        );
        assert!(plan.indices.is_empty());
        assert!(plan.probes.is_empty());
    }
}

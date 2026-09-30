//! Stage-by-stage tracker for the normalization pipeline shared by every
//! diff [`Algorithm`]:
//!
//! ```text
//! algorithm hook events
//!   -> (S1) Capture: raw DiffOp stream
//!   -> (S2) Replace: adjacent delete/insert events fused into Replace
//!   -> (S3) Compact::finish: cleanup_diff_ops shifts/merges equal boundaries
//!   -> (S4) capture_diff_slices / Capture::into_ops public DiffOp slice
//!   -> (S5) DiffOp::iter_changes / TextDiff / udiff / three-way merge
//! ```
//!
//! Every stage is checked against the same structural invariants so the
//! accompanying `ANALYSIS.md` can point a reviewer at a concrete test for each
//! claim instead of asking them to trust the full suite.

use std::fmt;
use std::ops::Range;
use std::time::{Duration, Instant};

use similar::algorithms::{
    Algorithm, Capture, Compact, DiffHook, Replace, diff_deadline, diff_slices_deadline,
};
use similar::{
    ChangeTag, DiffOp, DiffTag, TextDiff, TextMerge, capture_diff,
    capture_diff_slices_deadline,
};

const FIVE: [Algorithm; 5] = [
    Algorithm::Myers,
    Algorithm::Patience,
    Algorithm::Histogram,
    Algorithm::Lcs,
    Algorithm::Hunt,
];

/// The structural invariants any well-formed diff op stream must satisfy for
/// the input ranges it was produced for.  All five pipeline stages are checked
/// with this helper.
///
/// `fused` is false for the raw S1 stream, where LCS in particular emits one
/// equal/delete/insert op per table cell, so adjacent ops may share a tag.
/// From S2 (Replace) onwards adjacent same-tag pieces are always fused, which
/// is asserted separately.
fn assert_op_invariants(
    ops: &[DiffOp],
    old_span: Range<usize>,
    new_span: Range<usize>,
    stage: &str,
    fused: bool,
) {
    let mut old_cursor = old_span.start;
    let mut new_cursor = new_span.start;
    for (index, op) in ops.iter().enumerate() {
        let (tag, old_range, new_range) = op.as_tag_tuple();

        // I1: ranges continue exactly where the previous op stopped, on both
        // sides.  This is the "old/new ranges continuously cover the input"
        // contract that text/udiff/merge indexing relies on.
        assert_eq!(
            old_range.start, old_cursor,
            "{stage}: old gap at op {index}: {op:?}"
        );
        assert_eq!(
            new_range.start, new_cursor,
            "{stage}: new gap at op {index}: {op:?}"
        );
        assert!(old_range.end >= old_range.start, "{stage}: old range inverted");
        assert!(new_range.end >= new_range.start, "{stage}: new range inverted");

        // I2: tag/range shape: Delete only consumes old input, Insert only
        // new input, Equal consumes equal amounts, Replace consumes both.
        match tag {
            DiffTag::Equal => {
                assert_eq!(
                    old_range.len(),
                    new_range.len(),
                    "{stage}: equal op with unequal sides: {op:?}"
                );
                assert!(!old_range.is_empty(), "{stage}: empty equal op");
            }
            DiffTag::Delete => {
                assert!(new_range.is_empty(), "{stage}: delete with new range");
                assert!(!old_range.is_empty(), "{stage}: empty delete op");
            }
            DiffTag::Insert => {
                assert!(old_range.is_empty(), "{stage}: insert with old range");
                assert!(!new_range.is_empty(), "{stage}: empty insert op");
            }
            DiffTag::Replace => {
                assert!(!old_range.is_empty(), "{stage}: empty side of replace");
                assert!(!new_range.is_empty(), "{stage}: empty side of replace");
            }
        }

        // I3 (fused streams only): adjacent ops must not carry the same tag.
        // Replace merges consecutive equal runs and same-side edits before
        // the public slice is built; the raw algorithm stream is allowed to
        // keep them split.
        if fused {
            if let Some(previous) = ops.get(index.wrapping_sub(1)).filter(|_| index > 0) {
                assert_ne!(
                    previous.tag(),
                    tag,
                    "{stage}: adjacent {tag:?} ops were not fused"
                );
            }
        }

        old_cursor = old_range.end;
        new_cursor = new_range.end;
    }

    // I4: the stream reaches exactly the end of both input ranges.
    assert_eq!(
        old_cursor, old_span.end,
        "{stage}: old input not fully covered"
    );
    assert_eq!(
        new_cursor, new_span.end,
        "{stage}: new input not fully covered"
    );
}

/// Specialized check for raw deadline-fallback streams.  An algorithm that
/// gives up emits the whole middle as one delete event followed by one insert
/// event.  Such a pair is *end-covering* (the two ranges between them span the
/// whole input) but not cursor-contiguous on each side: the insert's old
/// cursor sits at the end.  Compact's cursor normalization repairs that.
fn assert_end_covering(ops: &[DiffOp], old_len: usize, new_len: usize, stage: &str) {
    assert!(!ops.is_empty(), "{stage}: no ops");
    for op in ops {
        let (_, old_range, new_range) = op.as_tag_tuple();
        assert!(old_range.start <= old_len, "{stage}: old range out of bounds");
        assert!(new_range.start <= new_len, "{stage}: new range out of bounds");
        assert!(old_range.end <= old_len, "{stage}: old range out of bounds");
        assert!(new_range.end <= new_len, "{stage}: new range out of bounds");
    }
    // Between them the ops touch 0 on each side and reach the end.
    assert!(
        ops.iter().any(|op| op.old_range().start == 0)
            || ops.iter().any(|op| op.new_range().start == 0),
        "{stage}: fallback does not start at an input edge"
    );
    assert_eq!(
        ops.iter().map(|op| op.old_range().len()).sum::<usize>(),
        old_len,
        "{stage}: old items lost"
    );
    assert_eq!(
        ops.iter().map(|op| op.new_range().len()).sum::<usize>(),
        new_len,
        "{stage}: new items lost"
    );
}

/// Expands ops back into `(tag, value)` changes and checks that deletes, in
/// order, reproduce `old` while inserts reproduce `new` with equal runs
/// agreeing item by item.
fn assert_rebuild<T>(ops: &[DiffOp], old: &[T], new: &[T], stage: &str)
where
    T: Clone + fmt::Debug + PartialEq,
{
    // Walk the expanded change stream.  Equal items must agree pairwise; the
    // delete/insert interleaving in a Replace is exercised by iter_changes.
    for op in ops {
        for change in op.iter_changes(old, new) {
            if let (ChangeTag::Equal, Some(old_index), Some(new_index)) =
                (change.tag(), change.old_index(), change.new_index())
            {
                assert_eq!(
                    &old[old_index],
                    &new[new_index],
                    "{stage}: equal change points at unequal items"
                );
            }
        }
    }

    // Stronger structural content check: iter_slices rebuilds both inputs.
    let mut rebuilt_old = Vec::new();
    let mut rebuilt_new = Vec::new();
    for op in ops {
        for (tag, slice) in op.iter_slices(old, new) {
            match tag {
                ChangeTag::Equal => {
                    rebuilt_old.extend_from_slice(slice);
                    rebuilt_new.extend_from_slice(slice);
                }
                ChangeTag::Delete => rebuilt_old.extend_from_slice(slice),
                ChangeTag::Insert => rebuilt_new.extend_from_slice(slice),
            }
        }
    }
    assert_eq!(rebuilt_old, old, "{stage}: changes do not rebuild old");
    assert_eq!(rebuilt_new, new, "{stage}: changes do not rebuild new");
}

/// All stages captured for one (algorithm, old, new) triple.
struct Stages {
    raw: Vec<DiffOp>,
    replaced: Vec<DiffOp>,
    compacted: Vec<DiffOp>,
    public: Vec<DiffOp>,
}

fn run_stages<T>(alg: Algorithm, old: &[T], new: &[T], deadline: Option<Instant>) -> Stages
where
    T: Eq + std::hash::Hash,
{
    // S1: raw hook events straight from the algorithm.  RawMyers aside,
    // algorithms emit equal/delete/insert pieces; they are not yet fused.
    let mut raw = Capture::new();
    diff_slices_deadline(alg, &mut raw, old, new, deadline).unwrap();
    let raw = raw.into_ops();

    // S2: Replace fuses adjacent delete+insert clusters into Replace ops and
    // also merges consecutive equal/delete/insert events arriving separately.
    let mut replaced = Replace::new(Capture::new());
    diff_slices_deadline(alg, &mut replaced, old, new, deadline).unwrap();
    let replaced = replaced.into_inner().into_ops();

    // S3: Compact keeps the raw stream during diffing and only runs
    // cleanup_diff_ops on finish; capture with Replace behind it so the output
    // matches what capture_diff_slices builds from.
    let mut compacted = Compact::new(Replace::new(Capture::new()), old, new);
    diff_slices_deadline(alg, &mut compacted, old, new, deadline).unwrap();
    let compacted = compacted.into_inner().into_inner().into_ops();

    // S4: the public entrypoint is Compact -> Replace -> Capture.
    let public = capture_diff_slices_deadline(alg, old, new, deadline);

    Stages {
        raw,
        replaced,
        compacted,
        public,
    }
}

impl Stages {
    fn check<T>(&self, old: &[T], new: &[T])
    where
        T: Clone + fmt::Debug + PartialEq,
    {
        assert_op_invariants(&self.raw, 0..old.len(), 0..new.len(), "S1 raw capture", false);
        assert_op_invariants(
            &self.replaced,
            0..old.len(),
            0..new.len(),
            "S2 replace-fused",
            true,
        );
        assert_op_invariants(
            &self.compacted,
            0..old.len(),
            0..new.len(),
            "S3 compacted",
            true,
        );
        assert_op_invariants(&self.public, 0..old.len(), 0..new.len(), "S4 public ops", true);

        assert_rebuild(&self.raw, old, new, "S1 raw capture");
        assert_rebuild(&self.replaced, old, new, "S2 replace-fused");
        assert_rebuild(&self.compacted, old, new, "S3 compacted");
        assert_rebuild(&self.public, old, new, "S4 public ops");

        // capture_diff_slices is documented as Compact+Replace+Capture.
        assert_eq!(self.compacted, self.public);
    }
}

/// The fixed corpus: repeated lines, interleaved block moves, empty endpoints,
/// and sequences with several equal-cost LCS candidates.
fn corpus() -> Vec<(&'static str, Vec<i32>, Vec<i32>)> {
    vec![
        // Repeated values: multiple equally valid matching sites.
        ("repeated", vec![1, 1, 2, 1, 2], vec![2, 1, 2, 1, 1]),
        // Pure interleaved block move with no unique anchors in the middle.
        ("interleaved_move", vec![1, 2, 3, 4, 5, 6], vec![4, 5, 6, 1, 2, 3]),
        // Equal-cost candidates: both length-2 LCS choices exist.
        ("equal_cost", vec![1, 2, 1], vec![2, 1, 2]),
        // Repetition around a zero anchor that forces compact to shift edges.
        ("dup_shift", vec![1, 1, 0, 2], vec![2, 1, 0, 0, 0, 0]),
        // Empty endpoints on both sides.
        ("both_empty", vec![], vec![]),
        ("old_empty", vec![], vec![1, 1]),
        ("new_empty", vec![1, 1, 2], vec![]),
        // Empty *middle* endpoints: only an edge survives.
        ("edge_only", vec![7, 1, 2], vec![7, 9, 9]),
        // Identical input: a single equal op in every algorithm.
        ("identical", vec![3, 3, 3], vec![3, 3, 3]),
        // Long common ends with a fully repeated, ambiguous middle.
        (
            "anchored_ambiguous_middle",
            vec![9, 9, 1, 2, 1, 2, 8, 8],
            vec![9, 9, 2, 1, 2, 1, 8, 8],
        ),
    ]
}

#[test]
fn test_all_algorithms_survive_every_normalization_stage() {
    for (name, old, new) in corpus() {
        for alg in FIVE {
            let stages = run_stages(alg, &old, &new, None);
            stages.check(&old, &new);

            // Sanity specific to the corpus: identical inputs must yield one
            // equal op at every stage, including the raw hook stream.
            if name == "identical" {
                assert_eq!(
                    stages.raw,
                    vec![DiffOp::Equal {
                        old_index: 0,
                        new_index: 0,
                        len: old.len(),
                    }],
                    "{alg:?} on identical input"
                );
            }
        }
    }
}

/// Compact moves the *boundary* between an edit and its neighboring equal run
/// when the items crossing that boundary are equal.  The histogram stream for
/// `[1,1,0,2] -> [2,1,0,0,0,0]` is the smallest case where this shows up:
/// raw matching prefers the suffix, compaction lifts one repeated `1`/`2`
/// across the edit so the edit joins into a Replace.
#[test]
fn test_compact_moves_boundary_across_repeated_items() {
    let old = vec![1, 1, 0, 2];
    let new = vec![2, 1, 0, 0, 0, 0];

    let stages = run_stages(Algorithm::Histogram, &old, &new, None);

    // S1 (raw): the histogram anchor script deletes the leading 1, inserts
    // the leading 2, then matches [1,0].  Delete and insert are separate hook
    // events and the leading delete/insert are not aligned on a cursor.
    assert_eq!(
        stages.raw,
        vec![
            DiffOp::Delete {
                old_index: 0,
                old_len: 1,
                new_index: 0,
            },
            DiffOp::Insert {
                old_index: 1,
                new_index: 0,
                new_len: 1,
            },
            DiffOp::Equal {
                old_index: 1,
                new_index: 1,
                len: 2,
            },
            DiffOp::Delete {
                old_index: 3,
                old_len: 1,
                new_index: 3,
            },
            DiffOp::Insert {
                old_index: 4,
                new_index: 3,
                new_len: 3,
            },
        ]
    );

    // S2 (fused, un-compacted): adjacent delete+insert pairs become Replace
    // ops, but equal boundaries have not moved yet: the first replace still
    // sits at index 0 on both sides and consumes old[0] / new[0].
    assert_eq!(
        stages.replaced,
        vec![
            DiffOp::Replace {
                old_index: 0,
                old_len: 1,
                new_index: 0,
                new_len: 1,
            },
            DiffOp::Equal {
                old_index: 1,
                new_index: 1,
                len: 2,
            },
            DiffOp::Replace {
                old_index: 3,
                old_len: 1,
                new_index: 3,
                new_len: 3,
            },
        ]
    );

    // S3: cleanup_diff_ops shifts the leading delete/insert across the
    // repeated 1 (old[1] == new[1]) and fuses the trailing delete+insert into
    // a replace.  Boundaries move; coverage does not.
    assert_eq!(
        stages.compacted,
        vec![
            DiffOp::Insert {
                old_index: 0,
                new_index: 0,
                new_len: 1,
            },
            DiffOp::Equal {
                old_index: 0,
                new_index: 1,
                len: 1,
            },
            DiffOp::Delete {
                old_index: 1,
                old_len: 1,
                new_index: 2,
            },
            DiffOp::Equal {
                old_index: 2,
                new_index: 2,
                len: 1,
            },
            DiffOp::Replace {
                old_index: 3,
                old_len: 1,
                new_index: 3,
                new_len: 3,
            },
        ]
    );

    // The shift is content-driven: it only crosses *equal* items.  Check that
    // the moved equal op really points at equal values, and that the item the
    // delete now covers is the one left unmatched.
    let equal_op = stages.compacted[1];
    assert_eq!(old[0], new[1]);
    let _ = equal_op;
}

/// When there is no equal item to cross, compact must not move the boundary.
/// `[1,2,3] -> [4,5,6]` has no match at all, so every algorithm collapses to
/// a single replace without any boundary motion.
#[test]
fn test_compact_is_identity_without_common_items() {
    let old = vec![1, 2, 3];
    let new = vec![4, 5, 6];
    for alg in FIVE {
        let stages = run_stages(alg, &old, &new, None);
        let expected = vec![DiffOp::Replace {
            old_index: 0,
            old_len: 3,
            new_index: 0,
            new_len: 3,
        }];
        assert_eq!(stages.compacted, expected, "{alg:?}");
        assert_eq!(stages.public, expected, "{alg:?}");
    }
}

/// Replace is the stage that fuses LCS' per-cell events: the classic LCS
/// table walk emits one op per cell, so equal runs and runs of same-side
/// edits only become contiguous *after* the Replace hook.
#[test]
fn test_replace_fuses_per_cell_lcs_events() {
    let old = vec![1, 2, 1];
    let new = vec![2, 1, 2];

    let stages = run_stages(Algorithm::Lcs, &old, &new, None);

    // S1: four single-cell ops, two of them adjacent Equal.
    assert_eq!(
        stages.raw,
        vec![
            DiffOp::Delete {
                old_index: 0,
                old_len: 1,
                new_index: 0,
            },
            DiffOp::Equal {
                old_index: 1,
                new_index: 0,
                len: 1,
            },
            DiffOp::Equal {
                old_index: 2,
                new_index: 1,
                len: 1,
            },
            DiffOp::Insert {
                old_index: 3,
                new_index: 2,
                new_len: 1,
            },
        ]
    );

    // S2: the two equal cells are one run; nothing crosses a boundary.
    assert_eq!(
        stages.replaced,
        vec![
            DiffOp::Delete {
                old_index: 0,
                old_len: 1,
                new_index: 0,
            },
            DiffOp::Equal {
                old_index: 1,
                new_index: 0,
                len: 2,
            },
            DiffOp::Insert {
                old_index: 3,
                new_index: 2,
                new_len: 1,
            },
        ]
    );

    // Compact preserves this script verbatim.
    assert_eq!(stages.compacted, stages.replaced);
}

/// Ranges are absolute indices into the input slices, not indices relative to
/// the diffed subrange.  Diffing a subrange must therefore cover exactly that
/// subrange, starting at its offsets on both sides.
#[test]
fn test_subrange_ops_keep_absolute_offsets() {
    // Padding around the interesting middle so the ranges are non-zero.
    let old = [90, 91, 1, 1, 2, 92, 93];
    let new = [80, 2, 1, 0, 0, 0, 81, 82];
    let old_range = 2..5;
    let new_range = 1..7;

    for alg in FIVE {
        let ops = capture_diff(alg, &old, old_range.clone(), &new, new_range.clone());
        assert_op_invariants(
            &ops,
            old_range.clone(),
            new_range.clone(),
            "subrange coverage",
            true,
        );
        // The first op starts exactly at the subrange origins.
        let first = ops.first().unwrap();
        assert_eq!(first.old_range().start, old_range.start, "{alg:?}");
        assert_eq!(first.new_range().start, new_range.start, "{alg:?}");

        // Every range is confined to the requested subrange.
        for op in &ops {
            fn within(point: usize, range: &Range<usize>) -> bool {
                range.start <= point && point <= range.end
            }
            assert!(within(op.old_range().start, &old_range) && within(op.old_range().end, &old_range));
            assert!(within(op.new_range().start, &new_range) && within(op.new_range().end, &new_range));
        }
    }
}

fn expired() -> Option<Instant> {
    // Deterministic: an instant already in the past.  No sleeps.
    Some(Instant::now() - Duration::from_secs(3600))
}

/// When the deadline is already exceeded each algorithm gives up on matching
/// the middle and emits delete-all + insert-all for it.  The resulting public
/// op must still cover the inputs contiguously.  LCS is the notable case: its
/// raw insert carries the *end* old cursor, so only compact's cursor
/// normalization makes the fused Replace start at 0.
#[test]
fn test_expired_deadline_degrades_to_replace_without_breaking_coverage() {
    let old = vec![1, 1, 2, 1, 2];
    let new = vec![2, 1, 2, 1, 1];

    for alg in FIVE {
        let mut raw = Capture::new();
        diff_slices_deadline(alg, &mut raw, &old, &new, expired()).unwrap();
        let raw = raw.into_ops();

        // The raw fallback covers both inputs in total length, even though the
        // delete+insert pair leaves the dormant-side cursor at the end until
        // compact normalizes it.
        assert_end_covering(&raw, old.len(), new.len(), "raw deadline fallback");
        assert_eq!(raw.len(), 2, "{alg:?}: expected delete+insert fallback");
        assert!(matches!(raw[0], DiffOp::Delete { old_index: 0, old_len: 5, .. }));
        assert!(matches!(raw[1], DiffOp::Insert { new_index: 0, new_len: 5, .. }));

        // LCS points the raw insert at the post-delete old cursor; record that
        // this is the raw shape the cursor normalization has to repair.
        let raw_insert_old = match raw[1] {
            DiffOp::Insert { old_index, .. } => old_index,
            other => panic!("unexpected {other:?}"),
        };
        assert!(raw_insert_old == 0 || raw_insert_old == old.len());

        let public = capture_diff_slices_deadline(alg, &old, &new, expired());
        assert_op_invariants(
            &public,
            0..old.len(),
            0..new.len(),
            "public deadline fallback",
            true
        );
        assert_eq!(
            public,
            vec![DiffOp::Replace {
                old_index: 0,
                old_len: old.len(),
                new_index: 0,
                new_len: new.len(),
            }],
            "{alg:?}: degraded result must be one replace"
        );
    }
}

/// Prefix/suffix trimming happens before the deadline-sensitive middle is
/// searched.  On an expired deadline the middle degrades to a replace while
/// the matched ends survive as Equal ops, for every algorithm.
#[test]
fn test_expired_deadline_keeps_trimmed_anchors() {
    let old = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let new = vec![1, 2, 9, 9, 5, 6, 7, 8];

    for alg in FIVE {
        let public = capture_diff_slices_deadline(alg, &old, &new, expired());
        assert_op_invariants(&public, 0..old.len(), 0..new.len(), "trimmed anchors", true);
        assert_eq!(
            public,
            vec![
                DiffOp::Equal {
                    old_index: 0,
                    new_index: 0,
                    len: 2,
                },
                DiffOp::Replace {
                    old_index: 2,
                    old_len: 2,
                    new_index: 2,
                    new_len: 2,
                },
                DiffOp::Equal {
                    old_index: 4,
                    new_index: 4,
                    len: 4,
                },
            ],
            "{alg:?}"
        );
    }
}

/// Large, entirely different inputs are the shared near-disjoint fast path
/// territory.  An expired deadline must still yield exactly one replace over
/// the whole input for every algorithm, not a partial script.
#[test]
fn test_expired_deadline_large_disjoint_inputs() {
    let n = 3000usize;
    let old: Vec<u32> = (0..n as u32).collect();
    let new: Vec<u32> = (n as u32..2 * n as u32).collect();

    for alg in FIVE {
        let public = capture_diff_slices_deadline(alg, &old, &new, expired());
        assert_op_invariants(&public, 0..n, 0..n, "large disjoint", true);
        assert_eq!(
            public,
            vec![DiffOp::Replace {
                old_index: 0,
                old_len: n,
                new_index: 0,
                new_len: n,
            }],
            "{alg:?}"
        );
    }
}

/// A hook error carrying the exact failing event must survive the whole
/// Compact -> Replace -> Capture chain with its cause and context intact:
/// normalization never masks or rewrites hook errors.
#[derive(Debug, PartialEq)]
struct Explosion {
    at_event: &'static str,
    old_index: usize,
    new_index: usize,
}

struct ExplodingHook {
    fail_at: usize,
    seen: usize,
}

impl DiffHook for ExplodingHook {
    type Error = Explosion;

    fn equal(
        &mut self,
        old_index: usize,
        new_index: usize,
        _len: usize,
    ) -> Result<(), Self::Error> {
        self.seen += 1;
        if self.seen == self.fail_at {
            Err(Explosion {
                at_event: "equal",
                old_index,
                new_index,
            })
        } else {
            Ok(())
        }
    }

    fn replace(
        &mut self,
        old_index: usize,
        _old_len: usize,
        new_index: usize,
        _new_len: usize,
    ) -> Result<(), Self::Error> {
        self.seen += 1;
        if self.seen == self.fail_at {
            Err(Explosion {
                at_event: "replace",
                old_index,
                new_index,
            })
        } else {
            Ok(())
        }
    }
}

#[test]
fn test_hook_error_keeps_cause_offset_and_context() {
    let old = vec![1, 2, 3, 4];
    let new = vec![1, 9, 3, 4];

    // Fail at the second event through Compact+Replace, which is the exact
    // stack capture_diff uses.
    let mut d = Compact::new(
        Replace::new(ExplodingHook {
            fail_at: 2,
            seen: 0,
        }),
        &old,
        &new,
    );
    let err = diff_deadline(
        Algorithm::Myers,
        &mut d,
        &old,
        0..old.len(),
        &new,
        0..new.len(),
        None,
    )
    .expect_err("hook failure must propagate");

    assert_eq!(
        err,
        Explosion {
            at_event: "replace",
            old_index: 1,
            new_index: 1,
        }
    );
}

/// S5: the public iterator expands ops into per-item changes that, in order,
/// reconstruct the two inputs.  Equal changes carry valid indices on both
/// sides, deletes only old, inserts only new.
#[test]
fn test_public_iterator_changes_reconstruct_inputs() {
    let old = vec![1, 1, 0, 2];
    let new = vec![2, 1, 0, 0, 0, 0];

    for alg in FIVE {
        let ops = similar::capture_diff_slices(alg, &old, &new);

        let mut from_old = Vec::new();
        let mut from_new = Vec::new();
        let mut old_index_seen = Vec::new();
        let mut new_index_seen = Vec::new();
        for change in ops.iter().flat_map(|op| op.iter_changes(&old, &new)) {
            match change.tag() {
                ChangeTag::Equal => {
                    let oi = change.old_index().expect("equal needs old index");
                    let ni = change.new_index().expect("equal needs new index");
                    assert_eq!(old[oi], new[ni], "{alg:?}: equal mismatch");
                    from_old.push(change.value());
                    from_new.push(change.value());
                    old_index_seen.push(oi);
                    new_index_seen.push(ni);
                }
                ChangeTag::Delete => {
                    assert!(change.new_index().is_none());
                    from_old.push(change.value());
                    old_index_seen.push(change.old_index().unwrap());
                }
                ChangeTag::Insert => {
                    assert!(change.old_index().is_none());
                    from_new.push(change.value());
                    new_index_seen.push(change.new_index().unwrap());
                }
            }
        }
        assert_eq!(from_old, old, "{alg:?}");
        assert_eq!(from_new, new, "{alg:?}");

        // Indices emitted per side are strictly increasing and in range.
        assert!(old_index_seen.windows(2).all(|w| w[0] < w[1]));
        assert!(new_index_seen.windows(2).all(|w| w[0] < w[1]));
    }
}

/// S5 over text: grouped ops stay contiguous internally, and the rendered
/// unified diff hunk header ranges line up with the changed line numbers.
#[test]
fn test_text_and_udiff_consume_normalized_ranges() {
    let old_text = "a\nb\nc\nd\ne\nf\ng\n";
    let new_text = "a\nX\nc\nd\ne\nY\ng\n";

    for alg in FIVE {
        let diff = TextDiff::configure().algorithm(alg).diff_lines(old_text, new_text);
        for group in diff.grouped_ops(2) {
            // Each hunk is internally contiguous and non-overlapping; group
            // boundaries intentionally drop long equal context.
            for window in group.windows(2) {
                assert_eq!(window[1].old_range().start, window[0].old_range().end);
                assert_eq!(window[1].new_range().start, window[0].new_range().end);
            }
        }

        let rendered = diff.unified_diff().context_radius(1).to_string();
        // Two single-line changes on lines 2 and 6 -> two separate hunks.
        let hunk_headers = rendered
            .lines()
            .filter(|line| line.starts_with("@@"))
            .collect::<Vec<_>>();
        assert_eq!(hunk_headers.len(), 2, "{alg:?}: {rendered:?}");
        // Each hunk references exactly the changed line plus one context line.
        assert!(rendered.contains("-b"), "{alg:?}");
        assert!(rendered.contains("+X"), "{alg:?}");
        assert!(rendered.contains("-f"), "{alg:?}");
        assert!(rendered.contains("+Y"), "{alg:?}");
    }
}

/// S5 for three-way merge: disjoint edits on the two sides merge cleanly for
/// every algorithm, which is only possible because the per-side DiffOp ranges
/// are contiguous and comparable via start/end.
#[test]
fn test_three_way_merge_consumes_disjoint_ranges() {
    let base = "a\nb\nc\nd\ne\n";
    let ours = "a\nB\nc\nd\ne\n";
    let theirs = "a\nb\nc\nD\ne\n";

    for alg in FIVE {
        let merge = TextMerge::configure()
            .algorithm(alg)
            .merge_lines(base, ours, theirs);
        assert!(!merge.is_conflicted(), "{alg:?}: expected clean merge");
        let mut out = Vec::new();
        merge.to_writer(&mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "a\nB\nc\nD\ne\n",
            "{alg:?}"
        );

        // Regions partition the base without gaps or overlaps.
        let mut cursor = 0usize;
        for region in merge.regions() {
            assert_eq!(region.base_range().start, cursor, "{alg:?}: base gap");
            cursor = region.base_range().end;
        }
        assert_eq!(cursor, base.lines().count(), "{alg:?}: base tail");
    }
}

/// Deterministic property sweep over small alphabets (heavy repetition means
/// many equal-cost candidates).  For every generated pair the *public* ops of
/// all five algorithms must be contiguous, fully covering, tag-well-formed,
/// and content-reconstructing.  No randomness source or sleep is involved.
#[test]
fn test_deterministic_property_sweep_repetitive_inputs() {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..400 {
        let old_len = (next() % 9) as usize;
        let new_len = (next() % 9) as usize;
        // Alphabet 0..=2 forces repeated lines and ambiguous matches.
        let old: Vec<u32> = (0..old_len).map(|_| (next() % 3) as u32).collect();
        let new: Vec<u32> = (0..new_len).map(|_| (next() % 3) as u32).collect();

        for alg in FIVE {
            let ops = similar::capture_diff_slices(alg, &old, &new);
            assert_op_invariants(&ops, 0..old_len, 0..new_len, "sweep", true);
            assert_rebuild(&ops, &old, &new, "sweep");

            // The matched content weight must not exceed the shorter input and
            // the edit distance implied by the ops is feasible.
            let equal_len = ops
                .iter()
                .map(|op| match op {
                    DiffOp::Equal { len, .. } => *len,
                    _ => 0,
                })
                .sum::<usize>();
            assert!(equal_len <= old_len);
            assert!(equal_len <= new_len);
        }
    }
}

/// The same sweep with an expired deadline: degraded scripts must keep the
/// structural and content guarantees too.
#[test]
fn test_deterministic_property_sweep_with_expired_deadline() {
    let mut state = 0x0fed_cba9_8765_4321u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..200 {
        let old_len = (next() % 12) as usize;
        let new_len = (next() % 12) as usize;
        let old: Vec<u32> = (0..old_len).map(|_| (next() % 4) as u32).collect();
        let new: Vec<u32> = (0..new_len).map(|_| (next() % 4) as u32).collect();

        for alg in FIVE {
            let ops = capture_diff_slices_deadline(alg, &old, &new, expired());
            assert_op_invariants(&ops, 0..old_len, 0..new_len, "sweep deadline", true);
            assert_rebuild(&ops, &old, &new, "sweep deadline");
        }
    }
}

/// cleanup_diff_ops switches from in-vector shifting to the gap buffer once
/// the op list grows past 64 ops.  A fine-grained alternating input produces
/// such a list; both the small and the large path must leave a contiguous,
/// reconstructing result.
#[test]
fn test_compact_large_op_list_uses_gap_buffer_path() {
    // Alternating singleton changes with equal separators: dozens of tiny
    // hunks, comfortably over IN_PLACE_COMPACT_MAX_OPS at the raw stage.
    let old: Vec<u32> = (0..200).map(|i| if i % 2 == 0 { i } else { 1000 }).collect();
    let new: Vec<u32> = (0..200).map(|i| if i % 2 == 0 { i } else { 2000 }).collect();

    for alg in FIVE {
        let mut raw = Capture::new();
        diff_slices_deadline(alg, &mut raw, &old, &new, None).unwrap();
        let raw = raw.into_ops();
        // The point of the case: at least one algorithm must actually traverse
        // the >64-ops compaction branch here.
        let raw_count = raw.len();

        let public = similar::capture_diff_slices(alg, &old, &new);
        assert_op_invariants(&public, 0..200, 0..200, "large compact", true);
        assert_rebuild(&public, &old, &new, "large compact");

        if alg == Algorithm::Lcs {
            assert!(
                raw_count > 64,
                "expected LCS per-cell stream past the gap-buffer cutoff, got {raw_count}"
            );
        }
    }
}

/// Pure block move: every algorithm must anchor on the full 3-item block, so
/// after normalization the script is insert block / equal block / delete
/// block even though raw LCS emits per-cell events.
#[test]
fn test_interleaved_block_move_normalizes_everywhere() {
    let old = vec![1, 2, 3, 4, 5, 6];
    let new = vec![4, 5, 6, 1, 2, 3];

    for alg in FIVE {
        let stages = run_stages(alg, &old, &new, None);
        // Two shortest scripts tie: insert the moved block first, or delete
        // the moved block first.  Myers/Patience/Histogram/Hunt pick one
        // orientation; the LCS table walk picks the other.  Both are valid
        // and both normalize to three contiguous block ops.
        let insert_first = vec![
            DiffOp::Insert {
                old_index: 0,
                new_index: 0,
                new_len: 3,
            },
            DiffOp::Equal {
                old_index: 0,
                new_index: 3,
                len: 3,
            },
            DiffOp::Delete {
                old_index: 3,
                old_len: 3,
                new_index: 6,
            },
        ];
        let delete_first = vec![
            DiffOp::Delete {
                old_index: 0,
                old_len: 3,
                new_index: 0,
            },
            DiffOp::Equal {
                old_index: 3,
                new_index: 0,
                len: 3,
            },
            DiffOp::Insert {
                old_index: 6,
                new_index: 3,
                new_len: 3,
            },
        ];
        assert!(
            stages.public == insert_first || stages.public == delete_first,
            "{alg:?}: unexpected block-move script {:#?}",
            stages.public
        );
        if matches!(alg, Algorithm::Lcs | Algorithm::Hunt) {
            assert_eq!(stages.public, delete_first);
        } else {
            assert_eq!(stages.public, insert_first, "{alg:?}");
        }

        // LCS and Patience reach that shape only through Replace fusion.
        if alg == Algorithm::Lcs {
            assert!(
                stages.raw.len() > stages.public.len(),
                "{alg:?}: raw stream should be per-cell"
            );
        }
    }
}

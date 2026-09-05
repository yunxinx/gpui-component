use std::{
    cmp::Ordering,
    ops::{Range, RangeInclusive},
};

use gpui::{Bounds, Pixels, Size, px};

/// Height used for blocks that have not been measured yet when no render has
/// supplied a real estimate (cache entries created off the UI thread, before
/// the first paint). The next render replaces it through [`BlockHeightCache::align`].
pub(crate) const DEFAULT_ESTIMATED_BLOCK_HEIGHT: Pixels = px(24.);

/// A height delta at or below this threshold is sub-pixel layout jitter, not
/// convergence: writing it back would only ask for a frame that lays out the
/// same thing again.
const MEASURE_EPSILON: f32 = 0.5;

/// One block's height: an estimate that holds the first frame, replaced by the
/// measured height once the block paints.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockHeight {
    pub(crate) estimated: Pixels,
    pub(crate) measured: Option<Pixels>,
}

impl BlockHeight {
    /// The height layout currently reserves: measurement wins once present.
    fn effective(self) -> Pixels {
        self.measured.unwrap_or(self.estimated)
    }
}

/// Per-block height bookkeeping for the windowed layout, aligned with the
/// parsed document's top-level blocks.
///
/// Estimates keep the document's total height honest before a block first
/// paints; measurements replace them as visible blocks converge. Entries are
/// spliced in place on streaming appends — a full reset would drop measured
/// heights for blocks the append never touched and make the scrollbar thumb
/// shrink mid-stream.
#[derive(Debug, Default)]
pub(crate) struct BlockHeightCache {
    entries: Vec<BlockHeight>,
    /// Quantized element width the current measurements were taken at.
    width_bucket: u16,
    typography_revision: u64,
}

/// Quantizes the element width so measurements survive sub-pixel and small
/// resizes; a bucket step means real reflow and invalidates them.
pub(crate) fn width_bucket(width: Pixels) -> u16 {
    const BUCKET_WIDTH: f32 = 64.;
    ((f32::from(width) / BUCKET_WIDTH).round().max(0.)) as u16
}

/// Where the text view sits in the window and the window viewport it is
/// visible through — the two inputs the windowed visible range derives from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowedLayout {
    pub(crate) element_bounds: Bounds<Pixels>,
    pub(crate) viewport_size: Size<Pixels>,
}

/// The element-space y range a windowed document has to lay out: the element's
/// intersection with the window viewport, widened by two viewport heights of
/// overdraw on each side so a scroll frame never lands on unpainted content
/// before the next frame can materialize it.
///
/// Returns `None` when the element lies entirely outside the viewport. An
/// element that has never been laid out (zero height) is assumed to sit at the
/// window top: the first frame materializes one viewport of blocks from the
/// start instead of nothing, and the measurement writebacks converge the rest.
pub(crate) fn windowed_visible_y_range(layout: WindowedLayout) -> Option<Range<Pixels>> {
    const OVERDRAW_VIEWPORTS: f32 = 2.0;
    let overdraw = layout.viewport_size.height * OVERDRAW_VIEWPORTS;
    let element_bounds = layout.element_bounds;

    if element_bounds.size.height <= px(0.) {
        return Some(px(0.)..(layout.viewport_size.height + overdraw).max(px(0.)));
    }

    let visible_top = element_bounds.origin.y.max(px(0.));
    let visible_bottom = element_bounds.bottom().min(layout.viewport_size.height);
    if visible_bottom <= visible_top {
        return None;
    }
    let top = (visible_top - element_bounds.origin.y - overdraw).max(px(0.));
    let bottom = visible_bottom - element_bounds.origin.y + overdraw;
    Some(top..bottom)
}

impl BlockHeightCache {
    /// The number of entries, matching the document's block count when aligned.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// How many blocks have a measurement written back. Test-only: it exposes
    /// how much of the document a frame actually laid out.
    #[cfg(test)]
    pub(crate) fn measured_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.measured.is_some())
            .count()
    }

    /// Rebuilds the cache for a document replacement: all blocks start from the
    /// estimate, because measurements belong to blocks that no longer exist.
    pub(crate) fn reset(&mut self, count: usize, estimated: Pixels) {
        self.entries.clear();
        self.entries.resize(
            count,
            BlockHeight {
                estimated,
                measured: None,
            },
        );
    }

    /// Aligns the cache length with the document without discarding heights.
    ///
    /// This is the render-path safety net for a `windowed` view whose cache has
    /// not been spliced yet (a late enable, or a parse that arrived before the
    /// flag was set).
    pub(crate) fn align(&mut self, count: usize, estimated: Pixels) {
        match count.cmp(&self.entries.len()) {
            Ordering::Greater => self.entries.resize(
                count,
                BlockHeight {
                    estimated,
                    measured: None,
                },
            ),
            Ordering::Less => self.entries.truncate(count),
            Ordering::Equal => {}
        }
    }

    /// Splices the cache after an incremental parse, keeping the measured
    /// heights of every unchanged block.
    ///
    /// `common_prefix` is the number of leading blocks the append left
    /// untouched. The block the append grew into — old entry `common_prefix`,
    /// re-parsed in place — keeps its measured height until it repaints:
    /// dropping it would let the total height, and the scrollbar thumb,
    /// shrink on every stream tick. Blocks past it are new or reparsed and
    /// start from the estimate.
    pub(crate) fn splice(&mut self, common_prefix: usize, new_count: usize, estimated: Pixels) {
        let tail_measured = self
            .entries
            .get(common_prefix)
            .and_then(|entry| entry.measured);

        self.entries.truncate(common_prefix);
        self.align(new_count, estimated);

        if let Some(measured) = tail_measured
            && new_count > common_prefix
            && let Some(entry) = self.entries.get_mut(common_prefix)
        {
            entry.measured = Some(measured);
        }
    }

    /// Drops measured heights when the layout inputs they were taken at no
    /// longer hold, keeping estimates so the total height stays plausible until
    /// the visible blocks measure again. Returns whether anything was dropped.
    pub(crate) fn invalidate(&mut self, width_bucket: u16, typography_revision: u64) -> bool {
        if self.width_bucket == width_bucket && self.typography_revision == typography_revision {
            return false;
        }
        for entry in &mut self.entries {
            entry.measured = None;
        }
        self.width_bucket = width_bucket;
        self.typography_revision = typography_revision;
        true
    }

    /// Records a measured height for a painted block. Returns whether the
    /// height the layout reserves moved by more than [`MEASURE_EPSILON`], so
    /// the caller can ask for one convergence frame.
    pub(crate) fn measure(&mut self, ix: usize, height: Pixels) -> bool {
        let Some(entry) = self.entries.get_mut(ix) else {
            return false;
        };
        if (height - entry.effective()).abs() <= px(MEASURE_EPSILON) {
            return false;
        }
        entry.measured = Some(height);
        true
    }

    /// The total height of a contiguous block range.
    pub(crate) fn sum_range(&self, range: Range<usize>) -> Pixels {
        let mut total = px(0.);
        for entry in self.entries.get(range).into_iter().flatten() {
            total += entry.effective();
        }
        total
    }

    /// The inclusive block-index range covering `y` (element-space).
    ///
    /// Returns `None` when the cache is empty or `y` reaches neither any
    /// block's top nor its bottom — an empty range renders only spacers.
    pub(crate) fn block_range_for_y(&self, y: Range<Pixels>) -> Option<RangeInclusive<usize>> {
        let mut acc = px(0.);
        let mut first = None;
        let mut last = 0;
        for (ix, entry) in self.entries.iter().enumerate() {
            let top = acc;
            acc += entry.effective();
            if acc <= y.start {
                continue;
            }
            if top >= y.end {
                break;
            }
            if first.is_none() {
                first = Some(ix);
            }
            last = ix;
        }
        first.map(|first| first..=last)
    }

    /// The index of the top-level block at element-space `y`.
    ///
    /// A `y` past the document resolves to the last block, mirroring how a
    /// selection endpoint dragged beyond the content pins to its end.
    pub(crate) fn block_ix_at_y(&self, y: Pixels) -> Option<usize> {
        if self.entries.is_empty() || y < px(0.) {
            return None;
        }
        let mut acc = px(0.);
        for (ix, entry) in self.entries.iter().enumerate() {
            acc += entry.effective();
            if y < acc {
                return Some(ix);
            }
        }
        Some(self.entries.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(count: usize, height: f32) -> BlockHeightCache {
        let mut cache = BlockHeightCache::default();
        cache.reset(count, px(height));
        cache
    }

    fn measure_all(cache: &mut BlockHeightCache, height: f32) {
        for ix in 0..cache.len() {
            cache.measure(ix, px(height));
        }
    }

    #[test]
    fn visible_y_range_covers_viewport_with_two_viewports_of_overdraw() {
        let viewport = gpui::size(px(800.), px(800.));

        // An element filling the viewport from the top: overdraw extends the
        // range two viewport heights past its bottom, clamped above zero.
        let at_top = Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(400.), px(400.)));
        assert_eq!(
            windowed_visible_y_range(WindowedLayout {
                element_bounds: at_top,
                viewport_size: viewport
            }),
            Some(px(0.)..px(2000.))
        );

        // An element straddling the viewport top (scrolled past): the range
        // starts at the element top and overdraws below.
        let straddling = Bounds::new(
            gpui::point(px(0.), px(-100.)),
            gpui::size(px(400.), px(900.)),
        );
        assert_eq!(
            windowed_visible_y_range(WindowedLayout {
                element_bounds: straddling,
                viewport_size: viewport
            }),
            Some(px(0.)..px(2500.))
        );

        // An element entering the viewport from below: overdraw reaches up to
        // its top edge only (clamped at zero).
        let entering = Bounds::new(
            gpui::point(px(0.), px(600.)),
            gpui::size(px(400.), px(900.)),
        );
        assert_eq!(
            windowed_visible_y_range(WindowedLayout {
                element_bounds: entering,
                viewport_size: viewport
            }),
            Some(px(0.)..px(1800.))
        );
    }

    #[test]
    fn visible_y_range_is_empty_outside_the_viewport() {
        let viewport = gpui::size(px(800.), px(800.));

        let above = Bounds::new(
            gpui::point(px(0.), px(-3000.)),
            gpui::size(px(400.), px(400.)),
        );
        assert_eq!(
            windowed_visible_y_range(WindowedLayout {
                element_bounds: above,
                viewport_size: viewport
            }),
            None
        );

        let below = Bounds::new(
            gpui::point(px(0.), px(1000.)),
            gpui::size(px(400.), px(400.)),
        );
        assert_eq!(
            windowed_visible_y_range(WindowedLayout {
                element_bounds: below,
                viewport_size: viewport
            }),
            None
        );
    }

    #[test]
    fn an_unlaid_out_element_assumes_the_window_top() {
        let viewport = gpui::size(px(800.), px(800.));
        assert_eq!(
            windowed_visible_y_range(WindowedLayout {
                element_bounds: Bounds::<Pixels>::default(),
                viewport_size: viewport
            }),
            Some(px(0.)..px(2400.))
        );
    }

    #[test]
    fn block_range_covers_head_middle_and_tail() {
        let cache = cache(10, 10.);

        // The first block only.
        assert_eq!(cache.block_range_for_y(px(0.)..px(10.)), Some(0..=0));
        // Blocks straddling the middle of the range.
        assert_eq!(cache.block_range_for_y(px(15.)..px(25.)), Some(1..=2));
        // Everything up to the last block's bottom.
        assert_eq!(cache.block_range_for_y(px(0.)..px(100.)), Some(0..=9));
        // Past the end: nothing.
        assert_eq!(cache.block_range_for_y(px(500.)..px(600.)), None);
        // Below the start: nothing.
        assert_eq!(cache.block_range_for_y(px(-50.)..px(-10.)), None);
    }

    #[test]
    fn block_ix_at_y_pins_past_the_end() {
        let cache = cache(4, 10.);

        assert_eq!(cache.block_ix_at_y(px(0.)), Some(0));
        assert_eq!(cache.block_ix_at_y(px(35.)), Some(3));
        // A drag past the document pins to its last block.
        assert_eq!(cache.block_ix_at_y(px(1000.)), Some(3));
        // Above the document has no block at all.
        assert_eq!(cache.block_ix_at_y(px(-1.)), None);
        assert_eq!(cache.block_ix_at_y(px(0.)), Some(0));
    }

    #[test]
    fn invalidate_drops_measured_only_on_a_real_mismatch() {
        let mut cache = cache(3, 20.);
        // Establish the measurement identity the heights were taken at.
        assert!(cache.invalidate(width_bucket(px(640.)), 7));
        measure_all(&mut cache, 40.);

        // Same width and typography: measurements hold.
        assert!(!cache.invalidate(width_bucket(px(640.)), 7));
        assert_eq!(cache.sum_range(0..3), px(120.));

        // A new width bucket clears measurements but keeps estimates.
        assert!(cache.invalidate(width_bucket(px(832.)), 7));
        assert_eq!(cache.sum_range(0..3), px(60.));

        // A typography revision change does the same.
        assert!(cache.invalidate(width_bucket(px(832.)), 8));
        assert_eq!(cache.sum_range(0..3), px(60.));

        // The stored identity is the quantized bucket, so a resize inside the
        // same bucket keeps measurements.
        assert!(!cache.invalidate(width_bucket(px(860.)), 8));
    }

    #[test]
    fn measure_writeback_ignores_sub_pixel_jitter() {
        let mut cache = cache(1, 20.);

        assert!(cache.measure(0, px(40.)));
        assert!(!cache.measure(0, px(40.2)));
        assert_eq!(cache.sum_range(0..1), px(40.));
        // Half a pixel is still jitter.
        assert!(!cache.measure(0, px(40.5)));
        assert!(cache.measure(0, px(41.)));
        assert_eq!(cache.sum_range(0..1), px(41.));

        // Out of range: nothing to write (the document changed under the frame).
        assert!(!cache.measure(9, px(40.)));
    }

    #[test]
    fn spacers_sum_to_the_total_height() {
        let mut cache = cache(10, 10.);
        cache.measure(3, px(25.));
        cache.measure(7, px(5.));
        let total = cache.sum_range(0..10);
        assert_eq!(total, px(8. * 10. + 25. + 5.));

        // Head spacer + visible blocks + tail spacer always rebuilds the total,
        // whichever range is materialized.
        let range = cache.block_range_for_y(px(35.)..px(75.)).unwrap();
        let head = cache.sum_range(0..*range.start());
        let tail = cache.sum_range(range.end() + 1..10);
        let visible = range
            .clone()
            .fold(px(0.), |acc, ix| acc + cache.sum_range(ix..ix + 1));
        assert_eq!(head + visible + tail, total);
    }

    #[test]
    fn streaming_appends_keep_total_height_monotonic_and_preserve_untouched_blocks() {
        let mut cache = cache(3, 20.);
        // Blocks 0 and 1 were painted and measured; block 2 is the streaming
        // tail, measured at a tall code block.
        cache.measure(0, px(40.));
        cache.measure(1, px(60.));
        cache.measure(2, px(200.));
        let total_before = cache.sum_range(0..3);

        // The append re-parses the tail block and adds 5 more. The common
        // prefix is everything before the tail.
        cache.splice(2, 8, px(20.));

        // Untouched blocks keep their measurements; the tail keeps its last
        // measured height until it repaints.
        assert_eq!(cache.sum_range(0..1), px(40.));
        assert_eq!(cache.sum_range(1..2), px(60.));
        assert_eq!(cache.sum_range(2..3), px(200.));
        // New blocks contribute estimates.
        assert_eq!(cache.sum_range(3..8), px(5. * 20.));
        assert!(cache.sum_range(0..8) >= total_before);

        // Re-measuring the grown tail only moves the total up.
        assert!(cache.measure(2, px(240.)));
        assert!(cache.sum_range(0..8) >= total_before);
    }

    #[test]
    fn a_reparse_that_changes_every_block_keeps_only_the_reparsed_tail_height() {
        let mut cache = cache(4, 20.);
        measure_all(&mut cache, 50.);

        // A reparse that changed every block (a mid-stream reference-definition
        // change forces one): the common prefix is empty, but the first block
        // is exactly what an append grew into, so its measured height is
        // retained until it repaints and the other three reset to estimates.
        cache.splice(0, 4, px(20.));
        assert_eq!(cache.sum_range(0..1), px(50.));
        assert_eq!(cache.sum_range(1..4), px(60.));

        // A reparse with no append semantics at all comes through `reset`.
        cache.reset(4, px(20.));
        assert_eq!(cache.sum_range(0..4), px(80.));
    }

    #[test]
    fn align_extends_with_estimates_and_truncates() {
        let mut cache = cache(2, 20.);
        cache.measure(0, px(30.));

        cache.align(4, px(10.));
        assert_eq!(cache.sum_range(0..1), px(30.));
        assert_eq!(cache.sum_range(2..4), px(20.));

        cache.align(1, px(10.));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.sum_range(0..1), px(30.));
    }

    #[test]
    fn reset_rebuilds_everything_from_the_estimate() {
        let mut cache = cache(2, 20.);
        measure_all(&mut cache, 50.);

        cache.reset(3, px(20.));
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.sum_range(0..3), px(60.));
    }

    #[test]
    fn width_bucket_survives_small_resizes() {
        assert_eq!(width_bucket(px(0.)), 0);
        assert_eq!(width_bucket(px(1000.)), 16);
        assert_eq!(width_bucket(px(1023.)), 16);
        assert_eq!(width_bucket(px(1025.)), 16);
        assert_eq!(width_bucket(px(1050.)), 16);
        assert_eq!(width_bucket(px(1080.)), 17);
    }
}

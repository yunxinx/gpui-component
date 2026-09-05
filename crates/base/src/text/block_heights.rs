use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{App, Pixels, SharedString, TextStyle, Window, px};

use crate::theme::ActiveTheme as _;

use super::TextViewStyle;

/// Initial height before the first layout supplies an inherited-text estimate.
pub(crate) const DEFAULT_ESTIMATED_BLOCK_HEIGHT: Pixels = px(24.);

#[derive(Clone, Copy)]
struct BlockHeight {
    height: Pixels,
    measured_at: Option<u64>,
}

impl BlockHeight {
    fn estimated(height: Pixels) -> Self {
        Self {
            height: height.max(px(0.)),
            measured_at: None,
        }
    }

    fn is_measured(self, epoch: u64) -> bool {
        self.measured_at == Some(epoch)
    }
}

struct MeasurementIdentity {
    estimated: Pixels,
    width: Pixels,
    typography: BlockTypography,
    style: TextViewStyle,
    heading_sizes: [Option<Pixels>; 6],
    renderer_revision: u64,
}

/// Inherited text and the code-block typography resolved outside that cascade.
#[derive(Clone, PartialEq)]
pub(super) struct BlockTypography {
    text_style: TextStyle,
    rem_size: Pixels,
    code_font: SharedString,
    code_font_size: Pixels,
}

impl BlockTypography {
    pub(super) fn capture(window: &Window, cx: &App) -> Self {
        let typography = &cx.theme().tokens.typography;
        Self {
            text_style: window.text_style(),
            rem_size: window.rem_size(),
            code_font: typography.mono.clone(),
            code_font_size: typography.mono_md.size,
        }
    }
}

#[derive(Default)]
struct BlockHeights {
    entries: Vec<BlockHeight>,
    /// Block starts followed by the total height. Accumulate at higher precision
    /// so fractional heights do not drift over very long documents.
    offsets: Vec<f64>,
    measured_count: usize,
    measurement_epoch: u64,
    identity: Option<MeasurementIdentity>,
}

impl BlockHeights {
    fn rebuild_offsets(&mut self, from: usize) {
        self.offsets.resize(self.entries.len() + 1, 0.);
        for ix in from..self.entries.len() {
            self.offsets[ix + 1] = self.offsets[ix] + f64::from(self.entries[ix].height);
        }
    }
}

/// Retained block geometry shared by a text state and its current element.
/// Measurements and prefix offsets have one owner; cloning does not copy them.
#[derive(Clone, Default)]
pub(crate) struct BlockHeightCache {
    inner: Rc<RefCell<BlockHeights>>,
}

impl BlockHeightCache {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    #[cfg(test)]
    pub(crate) fn measured_count(&self) -> usize {
        self.inner.borrow().measured_count
    }

    pub(crate) fn is_complete(&self) -> bool {
        let inner = self.inner.borrow();
        inner.measured_count == inner.entries.len()
    }

    /// Replaces the document and discards its previous measurement identity.
    pub(crate) fn reset(&self, count: usize, estimated: Pixels) {
        let mut inner = self.inner.borrow_mut();
        inner.entries.clear();
        inner
            .entries
            .resize(count, BlockHeight::estimated(estimated));
        inner.measured_count = 0;
        inner.measurement_epoch = 0;
        inner.identity = None;
        inner.rebuild_offsets(0);
    }

    /// Aligns length without changing the identity or existing measurements.
    pub(crate) fn align(&self, count: usize, estimated: Pixels) -> bool {
        let mut inner = self.inner.borrow_mut();
        let old_count = inner.entries.len();
        if old_count == count {
            return false;
        }

        if count < old_count {
            let epoch = inner.measurement_epoch;
            inner.measured_count -= inner.entries[count..]
                .iter()
                .filter(|entry| entry.is_measured(epoch))
                .count();
        }
        inner
            .entries
            .resize(count, BlockHeight::estimated(estimated));
        inner.rebuild_offsets(old_count.min(count));
        true
    }

    /// Retains only the semantically unchanged prefix after a parse update.
    /// The first changed block's old height remains an estimate until measured.
    pub(crate) fn splice(&self, common_prefix: usize, new_count: usize, estimated: Pixels) {
        let mut inner = self.inner.borrow_mut();
        let estimated = inner
            .identity
            .as_ref()
            .map_or(estimated, |identity| identity.estimated);
        let prefix = common_prefix.min(inner.entries.len()).min(new_count);
        let tail_estimate = inner.entries.get(prefix).map(|entry| entry.height);
        let epoch = inner.measurement_epoch;
        inner.measured_count -= inner.entries[prefix..]
            .iter()
            .filter(|entry| entry.is_measured(epoch))
            .count();
        inner.entries.truncate(prefix);
        inner
            .entries
            .resize(new_count, BlockHeight::estimated(estimated));
        if let Some(height) = tail_estimate
            && let Some(entry) = inner.entries.get_mut(prefix)
        {
            entry.height = height;
        }
        inner.rebuild_offsets(prefix);
    }

    /// Establishes the inputs under which measured heights remain valid.
    /// Returns true for a width, typography, or rich-style change, which also
    /// refreshes estimates. A renderer-only change retains effective heights
    /// as provisional estimates and returns false because it may not reflow.
    pub(crate) fn prepare(
        &self,
        count: usize,
        width: Pixels,
        typography: &BlockTypography,
        style: &TextViewStyle,
        renderer_revision: u64,
    ) -> bool {
        let estimated = (typography
            .text_style
            .line_height_in_pixels(typography.rem_size)
            .max(px(1.))
            + style.paragraph_gap().to_pixels(typography.rem_size))
        .max(px(0.));
        let heading_sizes = std::array::from_fn(|ix| style.heading_font_size(ix as u8 + 1));
        let same_layout = self
            .inner
            .borrow()
            .identity
            .as_ref()
            .is_some_and(|previous| {
                previous.estimated == estimated
                    && previous.width == width
                    && previous.typography == *typography
                    && previous.heading_sizes == heading_sizes
                    && previous.style == *style
            });
        if same_layout {
            let mut inner = self.inner.borrow_mut();
            let renderer_changed = inner
                .identity
                .as_ref()
                .is_some_and(|previous| previous.renderer_revision != renderer_revision);
            if renderer_changed {
                // Older epochs keep their geometry without remaining measured.
                inner.measurement_epoch = inner.measurement_epoch.wrapping_add(1);
                inner.measured_count = 0;
                if let Some(identity) = &mut inner.identity {
                    identity.renderer_revision = renderer_revision;
                }
            }
            drop(inner);
            self.align(count, estimated);
            return false;
        }

        let mut inner = self.inner.borrow_mut();
        inner.identity = Some(MeasurementIdentity {
            estimated,
            width,
            typography: typography.clone(),
            style: style.clone(),
            heading_sizes,
            renderer_revision,
        });
        inner
            .entries
            .resize(count, BlockHeight::estimated(estimated));
        inner.entries.fill(BlockHeight::estimated(estimated));
        inner.measured_count = 0;
        inner.rebuild_offsets(0);
        true
    }

    #[cfg(test)]
    pub(crate) fn measure(&self, ix: usize, height: Pixels) -> bool {
        self.measure_many([(ix, height)])
    }

    /// Records one frame's measurements and rebuilds the affected offsets once.
    /// Newly measured blocks count as a change even when the estimate was exact.
    pub(crate) fn measure_many(
        &self,
        measurements: impl IntoIterator<Item = (usize, Pixels)>,
    ) -> bool {
        let mut inner = self.inner.borrow_mut();
        let epoch = inner.measurement_epoch;
        let mut first_height_change = inner.entries.len();
        let mut changed = false;

        for (ix, height) in measurements {
            let Some(entry) = inner.entries.get_mut(ix) else {
                continue;
            };
            let height = height.max(px(0.));
            let was_measured = entry.is_measured(epoch);
            if was_measured && entry.height == height {
                continue;
            }

            if entry.height != height {
                first_height_change = first_height_change.min(ix);
            }
            entry.height = height;
            entry.measured_at = Some(epoch);
            inner.measured_count += usize::from(!was_measured);
            changed = true;
        }

        if first_height_change < inner.entries.len() {
            inner.rebuild_offsets(first_height_change);
        }
        changed
    }

    pub(crate) fn total_height(&self) -> Pixels {
        self.inner
            .borrow()
            .offsets
            .last()
            .copied()
            .map(Pixels::from)
            .unwrap_or(px(0.))
    }

    /// Returns a contiguous height sum without walking its blocks.
    pub(crate) fn sum_range(&self, range: Range<usize>) -> Pixels {
        let inner = self.inner.borrow();
        if range.start > range.end {
            return px(0.);
        }
        inner
            .offsets
            .get(range.start)
            .zip(inner.offsets.get(range.end))
            .map(|(start, end)| Pixels::from(*end - *start))
            .unwrap_or(px(0.))
    }

    /// Finds the first block touching a visible range's start. Zero-height
    /// blocks at that boundary must remain eligible for materialization.
    pub(crate) fn first_block_for_y(&self, y: Pixels) -> Option<usize> {
        let inner = self.inner.borrow();
        if inner.entries.is_empty() || y < px(0.) {
            return None;
        }
        Some(
            inner.offsets[1..]
                .partition_point(|bottom| Pixels::from(*bottom) < y)
                .min(inner.entries.len() - 1),
        )
    }

    /// Resolves a selection endpoint, pinning positions past the document to
    /// its last block. Negative positions and an empty document have no block.
    pub(crate) fn block_ix_at_y(&self, y: Pixels) -> Option<usize> {
        let inner = self.inner.borrow();
        if inner.entries.is_empty() || y < px(0.) {
            return None;
        }
        Some(
            inner.offsets[1..]
                .partition_point(|bottom| Pixels::from(*bottom) <= y)
                .min(inner.entries.len() - 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{FontStyle, FontWeight, rems};

    use super::*;

    fn cache(count: usize, height: f32) -> BlockHeightCache {
        let cache = BlockHeightCache::default();
        cache.reset(count, px(height));
        cache
    }

    fn styles() -> (TextStyle, TextViewStyle) {
        (
            TextStyle {
                font_size: px(14.).into(),
                line_height: px(16.).into(),
                ..Default::default()
            },
            TextViewStyle::default().with_paragraph_gap(rems(0.25)),
        )
    }

    fn typography(text_style: &TextStyle, rem_size: Pixels) -> BlockTypography {
        BlockTypography {
            text_style: text_style.clone(),
            rem_size,
            code_font: "monospace".into(),
            code_font_size: px(13.),
        }
    }

    #[test]
    fn measurements_share_geometry_and_track_exact_estimates() {
        let cache = cache(3, 20.);
        let shared = cache.clone();
        assert!(!cache.is_complete());
        assert!(shared.measure_many([(0, px(20.)), (1, px(20.)), (2, px(20.))]));
        assert!(cache.is_complete());
        assert_eq!(cache.measured_count(), 3);
        assert_eq!(cache.total_height(), px(60.));
        assert!(!cache.measure_many([(0, px(20.)), (1, px(20.)), (2, px(20.))]));
        assert!(cache.measure(1, px(20.25)));
        assert_eq!(shared.total_height(), px(60.25));
        assert_eq!(shared.sum_range(2..3), px(20.));
        assert!(!cache.measure(9, px(40.)));
    }

    #[test]
    fn exact_width_invalidates_measurements_and_same_inputs_preserve_them() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        assert!(cache.prepare(3, px(671.), &typography(&text_style, px(16.)), &style, 0));
        assert!(cache.measure_many((0..3).map(|ix| (ix, px(40.)))));
        assert!(!cache.prepare(3, px(671.), &typography(&text_style, px(16.)), &style, 0));
        assert_eq!(cache.total_height(), px(120.));
        assert_eq!(cache.measured_count(), 3);

        for width in [608., 608.25] {
            assert!(cache.prepare(3, px(width), &typography(&text_style, px(16.)), &style, 0));
            assert_eq!(cache.measured_count(), 0);
            assert_eq!(cache.total_height(), px(60.));
            cache.measure_many((0..3).map(|ix| (ix, px(40.))));
        }
    }

    #[test]
    fn inherited_typography_rem_and_estimates_invalidate_all_blocks() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        let mut current_text_style = text_style.clone();
        cache.prepare(
            3,
            px(640.),
            &typography(&current_text_style, px(16.)),
            &style,
            0,
        );

        for (next_text_style, expected_height) in [
            (
                TextStyle {
                    font_size: px(28.).into(),
                    ..text_style.clone()
                },
                px(60.),
            ),
            (
                TextStyle {
                    line_height: px(40.).into(),
                    ..text_style.clone()
                },
                px(132.),
            ),
            (
                TextStyle {
                    font_family: "monospace".into(),
                    ..text_style.clone()
                },
                px(60.),
            ),
            (
                TextStyle {
                    font_weight: FontWeight::BOLD,
                    font_style: FontStyle::Italic,
                    ..text_style.clone()
                },
                px(60.),
            ),
        ] {
            cache.measure_many((0..3).map(|ix| (ix, px(40.))));
            assert!(cache.prepare(
                3,
                px(640.),
                &typography(&next_text_style, px(16.)),
                &style,
                0
            ));
            assert_eq!(cache.measured_count(), 0);
            assert_eq!(cache.total_height(), expected_height);
            current_text_style = next_text_style;
        }

        cache.measure_many((0..3).map(|ix| (ix, px(40.))));
        assert!(cache.prepare(
            3,
            px(640.),
            &typography(&current_text_style, px(32.)),
            &style,
            0
        ));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(72.));
        cache.measure(0, px(60.));
        let style = style.with_paragraph_gap(rems(1.));
        assert!(cache.prepare(
            3,
            px(640.),
            &typography(&current_text_style, px(32.)),
            &style,
            0
        ));
        assert_eq!(cache.total_height(), px(144.));
    }

    #[test]
    fn rich_style_reflows_while_renderer_changes_retain_provisional_heights() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        cache.prepare(3, px(640.), &typography(&text_style, px(16.)), &style, 0);
        cache.measure_many((0..3).map(|ix| (ix, px(40.))));

        let style = style.with_paragraph_gap(rems(0.5));
        assert!(cache.prepare(3, px(640.), &typography(&text_style, px(16.)), &style, 0));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(72.));
        cache.measure_many((0..3).map(|ix| (ix, px(40.))));
        assert!(!cache.prepare(3, px(640.), &typography(&text_style, px(16.)), &style, 1));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(120.));
    }

    #[test]
    fn code_typography_invalidates_offscreen_measurements() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        let mut typography = typography(&text_style, px(16.));
        cache.prepare(3, px(640.), &typography, &style, 0);
        cache.measure_many((0..3).map(|ix| (ix, px(40.))));

        typography.code_font = "another-monospace".into();
        assert!(cache.prepare(3, px(640.), &typography, &style, 0));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(60.));
        cache.measure_many((0..3).map(|ix| (ix, px(40.))));

        typography.code_font_size = px(28.);
        assert!(cache.prepare(3, px(640.), &typography, &style, 0));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(60.));
        assert!(!cache.prepare(3, px(640.), &typography, &style, 0));
    }

    #[test]
    fn equivalent_renderer_replacements_keep_frame_results_stable() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        let typography = typography(&text_style, px(16.));
        cache.prepare(3, px(640.), &typography, &style, 0);
        cache.measure_many((0..3).map(|ix| (ix, px(40.))));

        for revision in 1..5 {
            assert!(!cache.prepare(3, px(640.), &typography, &style, revision));
            assert_eq!(cache.measured_count(), 0);
            assert!(cache.measure(0, px(40.)));
            assert_eq!(cache.total_height(), px(120.));
            assert!(!cache.is_complete());
        }

        assert!(!cache.prepare(3, px(640.), &typography, &style, 4));
        assert_eq!(cache.measured_count(), 1);
        cache.measure_many([(1, px(50.)), (2, px(60.))]);
        assert!(cache.is_complete());
        assert_eq!(cache.total_height(), px(150.));
    }

    #[test]
    fn stale_measurement_epochs_do_not_leak_into_splice_or_resize_validity() {
        let cache = cache(4, 20.);
        let (text_style, style) = styles();
        let typography = typography(&text_style, px(16.));
        cache.prepare(4, px(640.), &typography, &style, 0);
        cache.measure_many((0..4).map(|ix| (ix, px(40.))));

        assert!(!cache.prepare(4, px(640.), &typography, &style, 1));
        assert_eq!(cache.measured_count(), 0);
        assert!(cache.align(2, px(20.)));
        assert_eq!(cache.total_height(), px(80.));
        assert_eq!(cache.measured_count(), 0);
        assert!(cache.measure(0, px(40.)));
        cache.splice(1, 3, px(20.));
        assert_eq!(cache.measured_count(), 1);
        assert_eq!(cache.total_height(), px(100.));

        assert!(!cache.prepare(3, px(640.), &typography, &style, 2));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(100.));
        cache.measure_many([(0, px(40.)), (1, px(40.)), (2, px(20.))]);
        assert!(cache.is_complete());

        assert!(!cache.prepare(3, px(640.), &typography, &style, 1));
        assert_eq!(cache.measured_count(), 0);
        assert!(cache.measure(0, px(40.)));
        assert!(!cache.measure(0, px(40.)));
        assert_eq!(cache.measured_count(), 1);
    }

    #[test]
    fn resolved_heading_sizes_are_snapshots_of_the_measurement() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let cache = cache(2, 20.);
        let (text_style, style) = styles();
        let scale = Arc::new(AtomicUsize::new(1));
        let style = style.with_heading_font_size({
            let scale = scale.clone();
            move |_, base| base * scale.load(Ordering::Relaxed) as f32
        });
        cache.prepare(2, px(640.), &typography(&text_style, px(16.)), &style, 0);
        cache.measure_many([(0, px(40.)), (1, px(50.))]);
        scale.store(2, Ordering::Relaxed);
        assert!(cache.prepare(2, px(640.), &typography(&text_style, px(16.)), &style, 0));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(40.));
    }

    #[test]
    fn splices_retain_only_unchanged_measurements_and_provisional_tail_geometry() {
        let cache = cache(4, 20.);
        cache.measure_many([(0, px(40.)), (1, px(60.)), (2, px(200.)), (3, px(80.))]);

        cache.splice(2, 8, px(30.));
        assert_eq!(cache.measured_count(), 2);
        assert_eq!(cache.sum_range(0..2), px(100.));
        assert_eq!(cache.sum_range(2..3), px(200.));
        assert_eq!(cache.sum_range(3..8), px(150.));
        assert!(!cache.is_complete());
        assert!(cache.measure(2, px(200.)));
        assert_eq!(cache.measured_count(), 3);

        cache.splice(0, 4, px(25.));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.sum_range(0..1), px(40.));
        assert_eq!(cache.sum_range(1..4), px(75.));
        cache.measure_many((0..4).map(|ix| (ix, px(25.))));
        assert!(cache.is_complete());

        cache.splice(2, 2, px(30.));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.measured_count(), 2);
        assert_eq!(cache.total_height(), px(50.));
        assert!(cache.is_complete());
    }

    #[test]
    fn appended_estimates_use_the_latest_inherited_layout() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        cache.prepare(3, px(640.), &typography(&text_style, px(48.)), &style, 0);
        cache.measure_many([(0, px(40.)), (1, px(60.)), (2, px(80.))]);
        cache.splice(2, 5, DEFAULT_ESTIMATED_BLOCK_HEIGHT);
        assert_eq!(cache.measured_count(), 2);
        assert_eq!(cache.sum_range(2..3), px(80.));
        assert_eq!(cache.sum_range(3..5), px(56.));
        assert!(!cache.prepare(5, px(640.), &typography(&text_style, px(48.)), &style, 0));
        assert_eq!(cache.sum_range(2..5), px(136.));
    }

    #[test]
    fn replacement_and_length_changes_keep_offsets_and_validity_aligned() {
        let cache = cache(3, 20.);
        let (text_style, style) = styles();
        cache.prepare(3, px(640.), &typography(&text_style, px(16.)), &style, 0);
        cache.measure_many([(0, px(40.)), (1, px(60.)), (2, px(80.))]);

        assert!(!cache.prepare(5, px(640.), &typography(&text_style, px(16.)), &style, 0));
        assert_eq!(cache.measured_count(), 3);
        assert_eq!(cache.total_height(), px(220.));
        assert!(cache.align(2, px(20.)));
        assert_eq!(cache.measured_count(), 2);
        assert_eq!(cache.total_height(), px(100.));

        cache.reset(2, px(30.));
        assert_eq!(cache.measured_count(), 0);
        assert_eq!(cache.total_height(), px(60.));
        assert!(cache.prepare(2, px(640.), &typography(&text_style, px(16.)), &style, 0));
        assert_eq!(cache.total_height(), px(40.));
        cache.reset(0, px(20.));
        assert!(cache.is_complete());
        assert_eq!(cache.total_height(), px(0.));
        assert_eq!(cache.block_ix_at_y(px(0.)), None);
    }

    #[test]
    fn prefix_search_matches_contiguous_heights_and_half_open_boundaries() {
        let cache = cache(5, 10.);
        cache.measure_many([(1, px(25.)), (3, px(0.))]);
        assert_eq!(cache.total_height(), px(55.));
        assert_eq!(cache.sum_range(0..0), px(0.));
        assert_eq!(cache.sum_range(1..4), px(35.));
        assert_eq!(cache.sum_range(0..9), px(0.));
        assert_eq!(cache.block_ix_at_y(px(-1.)), None);
        assert_eq!(cache.block_ix_at_y(px(0.)), Some(0));
        assert_eq!(cache.block_ix_at_y(px(9.)), Some(0));
        assert_eq!(cache.block_ix_at_y(px(10.)), Some(1));
        assert_eq!(cache.block_ix_at_y(px(34.)), Some(1));
        assert_eq!(cache.block_ix_at_y(px(35.)), Some(2));
        assert_eq!(cache.block_ix_at_y(px(45.)), Some(4));
        assert_eq!(cache.block_ix_at_y(px(1000.)), Some(4));
        assert_eq!(cache.first_block_for_y(px(-1.)), None);
        assert_eq!(cache.first_block_for_y(px(0.)), Some(0));
        assert_eq!(cache.first_block_for_y(px(45.)), Some(2));
        assert_eq!(cache.first_block_for_y(px(1000.)), Some(4));

        for first in 0..5 {
            for end in first + 1..=5 {
                assert_eq!(
                    cache.sum_range(0..first)
                        + cache.sum_range(first..end)
                        + cache.sum_range(end..5),
                    cache.total_height()
                );
            }
        }

        cache.reset(3, px(0.));
        assert_eq!(cache.first_block_for_y(px(0.)), Some(0));
        assert_eq!(cache.block_ix_at_y(px(0.)), Some(2));
        cache.reset(0, px(0.));
        assert_eq!(cache.first_block_for_y(px(0.)), None);
    }

    #[test]
    fn long_document_offsets_do_not_accumulate_pixel_rounding() {
        let count = 100_000;
        let height = px(12.3);
        let cache = cache(count, f32::from(height));
        let expected = Pixels::from(f64::from(height) * count as f64);
        assert_eq!(cache.total_height(), expected);
        assert_eq!(cache.sum_range(count - 1..count), height);
        assert_eq!(
            cache.block_ix_at_y(cache.sum_range(0..count - 1)),
            Some(count - 1)
        );

        cache.measure_many([(0, px(10_000.)), (50_000, px(100_000.))]);
        let expected = Pixels::from(f64::from(height) * (count - 2) as f64 + 110_000.);
        assert_eq!(cache.total_height(), expected);
        assert_eq!(cache.sum_range(count - 1..count), height);
        assert_eq!(
            cache.block_ix_at_y(cache.sum_range(0..count - 1)),
            Some(count - 1)
        );
    }
}

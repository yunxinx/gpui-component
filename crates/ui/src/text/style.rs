use std::sync::Arc;

use gpui::{FontWeight, Pixels, Rems, StyleRefinement, px, rems};

use crate::highlighter::HighlightTheme;

/// TextViewStyle used to customize the style for [`TextView`].
#[derive(Clone)]
pub struct TextViewStyle {
    /// Gap of each paragraphs, default is 1 rem.
    pub paragraph_gap: Rems,
    /// Base font size for headings, default is 14px.
    pub heading_base_font_size: Pixels,
    /// Function to calculate heading font size based on heading level (1-6).
    ///
    /// The first parameter is the heading level (1-6), the second parameter is the base font size.
    /// The second parameter is the base font size.
    pub heading_font_size: Option<Arc<dyn Fn(u8, Pixels) -> Pixels + Send + Sync + 'static>>,
    /// Highlight theme for code blocks. Default: [`HighlightTheme::default_light()`]
    pub highlight_theme: Arc<HighlightTheme>,
    /// The style refinement for code blocks.
    pub code_block: StyleRefinement,
    /// Style refinement applied to the table container (the bordered wrapper
    /// in wrap mode, the scroll viewport in horizontal-scroll mode).
    ///
    /// Set `overflow_x: scroll` here to keep table cells on a single line and
    /// scroll the table horizontally instead of wrapping cell content, e.g.
    /// `TextViewStyle::default().table({ let mut s = StyleRefinement::default(); s.overflow.x = Some(Overflow::Scroll); s })`.
    pub table: StyleRefinement,
    /// Style refinement applied to each table cell.
    pub table_cell: StyleRefinement,
    pub is_dark: bool,
}

/// Resolved typography for one Markdown heading level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextViewHeadingStyle {
    /// Resolved heading font size.
    pub font_size: Pixels,
    /// Heading font weight.
    pub font_weight: FontWeight,
    /// Bottom padding used by the native heading renderer.
    pub padding_bottom: Rems,
}

impl PartialEq for TextViewStyle {
    fn eq(&self, other: &Self) -> bool {
        self.paragraph_gap == other.paragraph_gap
            && self.heading_base_font_size == other.heading_base_font_size
            && self.highlight_theme == other.highlight_theme
    }
}

impl Default for TextViewStyle {
    fn default() -> Self {
        Self {
            paragraph_gap: rems(1.),
            heading_base_font_size: px(14.),
            heading_font_size: None,
            highlight_theme: HighlightTheme::default_light().clone(),
            code_block: StyleRefinement::default(),
            table: StyleRefinement::default(),
            table_cell: StyleRefinement::default(),
            is_dark: false,
        }
    }
}

impl TextViewStyle {
    /// Resolve the same heading typography and spacing used by the native
    /// Markdown renderer.
    ///
    /// Block plugins that replace a heading can use this to preserve native
    /// heading behavior without duplicating the level switch.
    pub fn heading_style(&self, level: u8) -> TextViewHeadingStyle {
        let (size, font_weight) = match level {
            1 => (rems(2.), FontWeight::BOLD),
            2 => (rems(1.5), FontWeight::SEMIBOLD),
            3 => (rems(1.25), FontWeight::SEMIBOLD),
            4 => (rems(1.125), FontWeight::SEMIBOLD),
            5 => (rems(1.), FontWeight::SEMIBOLD),
            6 => (rems(1.), FontWeight::MEDIUM),
            _ => (rems(1.), FontWeight::NORMAL),
        };
        let font_size = self.heading_font_size.as_ref().map_or_else(
            || size.to_pixels(self.heading_base_font_size),
            |resolve| resolve(level, self.heading_base_font_size),
        );

        TextViewHeadingStyle {
            font_size,
            font_weight,
            padding_bottom: rems(0.3),
        }
    }

    /// Set paragraph gap, default is 1 rem.
    pub fn paragraph_gap(mut self, gap: Rems) -> Self {
        self.paragraph_gap = gap;
        self
    }

    pub fn heading_font_size<F>(mut self, f: F) -> Self
    where
        F: Fn(u8, Pixels) -> Pixels + Send + Sync + 'static,
    {
        self.heading_font_size = Some(Arc::new(f));
        self
    }

    /// Set style for code blocks.
    pub fn code_block(mut self, style: StyleRefinement) -> Self {
        self.code_block = style;
        self
    }

    /// Set extra style for the table container.
    ///
    /// Set `overflow_x: scroll` on the refinement to make wide tables scroll
    /// horizontally (cells stop wrapping) instead of shrinking to fit.
    pub fn table(mut self, style: StyleRefinement) -> Self {
        self.table = style;
        self
    }

    /// Set extra style for each table cell.
    pub fn table_cell(mut self, style: StyleRefinement) -> Self {
        self.table_cell = style;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_style_resolves_native_levels_and_custom_font_sizes() {
        let style = TextViewStyle::default();
        assert_eq!(
            style.heading_style(1),
            TextViewHeadingStyle {
                font_size: px(28.),
                font_weight: FontWeight::BOLD,
                padding_bottom: rems(0.3),
            }
        );
        assert_eq!(
            style.heading_style(6),
            TextViewHeadingStyle {
                font_size: px(14.),
                font_weight: FontWeight::MEDIUM,
                padding_bottom: rems(0.3),
            }
        );

        let custom = TextViewStyle::default().heading_font_size(|level, _| px(level as f32));
        assert_eq!(custom.heading_style(3).font_size, px(3.));
    }
}

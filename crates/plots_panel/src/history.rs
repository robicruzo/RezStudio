//! The plots R has drawn this session, newest last, and which one is shown.

use runtime::comms::PlotRenderSettings;

pub struct Plot<I> {
    pub comm_id: String,
    pub image: Option<I>,
    /// The settings `image` was drawn with, when known.
    pub rendered: Option<PlotRenderSettings>,
    /// False once R can't redraw it, because Ark closed its comm or R stopped.
    pub live: bool,
    /// Which render `image` came from. Renders decode in the background and
    /// can finish out of order; an older one must not replace a newer one.
    sequence: u64,
}

pub struct PlotHistory<I> {
    plots: Vec<Plot<I>>,
    current: usize,
}

impl<I> Default for PlotHistory<I> {
    fn default() -> Self {
        Self {
            plots: Vec::new(),
            current: 0,
        }
    }
}

impl<I> PlotHistory<I> {
    pub fn is_empty(&self) -> bool {
        self.plots.is_empty()
    }

    pub fn current(&self) -> Option<&Plot<I>> {
        self.plots.get(self.current)
    }

    /// "3 / 7": the shown plot's position, counting from the oldest.
    pub fn position_label(&self) -> Option<String> {
        self.current()
            .map(|_| format!("{} / {}", self.current + 1, self.plots.len()))
    }

    pub fn contains(&self, comm_id: &str) -> bool {
        self.plots.iter().any(|plot| plot.comm_id == comm_id)
    }

    /// Adds a new plot and shows it.
    pub fn add(&mut self, comm_id: String) {
        self.plots.push(Plot {
            comm_id,
            image: None,
            rendered: None,
            live: true,
            sequence: 0,
        });
        self.current = self.plots.len() - 1;
    }

    /// Stores a render of the plot, returning the image it replaces. A render
    /// older than the plot's current image is handed back instead.
    pub fn set_image(
        &mut self,
        comm_id: &str,
        image: I,
        rendered: Option<PlotRenderSettings>,
        sequence: u64,
    ) -> Option<I> {
        let Some(plot) = self.plots.iter_mut().find(|plot| plot.comm_id == comm_id) else {
            return Some(image);
        };
        if sequence < plot.sequence {
            return Some(image);
        }
        plot.sequence = sequence;
        plot.rendered = rendered;
        plot.image.replace(image)
    }

    pub fn image_mut(&mut self, comm_id: &str) -> Option<&mut I> {
        self.plots
            .iter_mut()
            .find(|plot| plot.comm_id == comm_id)
            .and_then(|plot| plot.image.as_mut())
    }

    /// Shows the plot; returns whether it exists.
    pub fn show(&mut self, comm_id: &str) -> bool {
        match self.plots.iter().position(|plot| plot.comm_id == comm_id) {
            Some(index) => {
                self.current = index;
                true
            }
            None => false,
        }
    }

    pub fn can_go(&self, forward: bool) -> bool {
        if forward {
            self.current + 1 < self.plots.len()
        } else {
            self.current > 0 && !self.plots.is_empty()
        }
    }

    /// Steps through history; returns whether the shown plot changed.
    pub fn go(&mut self, forward: bool) -> bool {
        if !self.can_go(forward) {
            return false;
        }
        if forward {
            self.current += 1;
        } else {
            self.current -= 1;
        }
        true
    }

    /// Removes the shown plot. The next newer one is shown, or the previous
    /// one when it was the newest.
    pub fn remove_current(&mut self) -> Option<Plot<I>> {
        if self.current >= self.plots.len() {
            return None;
        }
        let removed = self.plots.remove(self.current);
        self.current = self.current.min(self.plots.len().saturating_sub(1));
        Some(removed)
    }

    pub fn clear(&mut self) -> Vec<Plot<I>> {
        self.current = 0;
        std::mem::take(&mut self.plots)
    }

    /// Forgets what the plot's image was drawn with, so it's drawn again.
    pub fn invalidate(&mut self, comm_id: &str) {
        if let Some(plot) = self.plots.iter_mut().find(|plot| plot.comm_id == comm_id) {
            plot.rendered = None;
        }
    }

    pub fn mark_closed(&mut self, comm_id: &str) {
        if let Some(plot) = self.plots.iter_mut().find(|plot| plot.comm_id == comm_id) {
            plot.live = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::comms::{PlotRenderFormat, PlotSize};

    fn settings(width: i64) -> Option<PlotRenderSettings> {
        Some(PlotRenderSettings {
            size: PlotSize { width, height: 300 },
            pixel_ratio: 2.0,
            format: PlotRenderFormat::Png,
        })
    }

    fn history(count: usize) -> PlotHistory<&'static str> {
        let mut history = PlotHistory::default();
        for index in 0..count {
            history.add(format!("plot-{index}"));
        }
        history
    }

    fn current<'a>(history: &'a PlotHistory<&'static str>) -> Option<&'a str> {
        history.current().map(|plot| plot.comm_id.as_str())
    }

    #[test]
    fn test_new_plots_are_shown_and_history_steps() {
        let mut history = history(3);
        assert_eq!(current(&history), Some("plot-2"));
        assert_eq!(history.position_label().as_deref(), Some("3 / 3"));
        assert!(!history.go(true));
        assert!(history.go(false));
        assert!(history.go(false));
        assert!(!history.go(false));
        assert_eq!(history.position_label().as_deref(), Some("1 / 3"));

        assert!(history.show("plot-1"));
        assert_eq!(current(&history), Some("plot-1"));
        assert!(!history.show("missing"));
        history.add("plot-3".into());
        assert_eq!(history.position_label().as_deref(), Some("4 / 4"));
    }

    #[test]
    fn test_removing_shows_a_neighbor() {
        let mut history = history(3);
        history.show("plot-1");
        assert_eq!(
            history.remove_current().map(|plot| plot.comm_id),
            Some("plot-1".into())
        );
        assert_eq!(current(&history), Some("plot-2"), "the next newer one");
        history.remove_current();
        assert_eq!(
            current(&history),
            Some("plot-0"),
            "the previous when it was the newest"
        );
        history.remove_current();
        assert!(history.is_empty());
        assert!(history.remove_current().is_none());
        assert_eq!(history.position_label(), None);
        assert!(!history.go(false) && !history.go(true));
    }

    #[test]
    fn test_late_renders_dont_replace_newer_ones() {
        let mut history = history(1);
        assert_eq!(history.set_image("plot-0", "small", settings(400), 1), None);
        assert_eq!(
            history.set_image("plot-0", "large", settings(800), 3),
            Some("small")
        );
        // The render asked for second finishes last.
        assert_eq!(
            history.set_image("plot-0", "medium", settings(600), 2),
            Some("medium")
        );
        let plot = history.current().unwrap();
        assert_eq!(plot.image, Some("large"));
        assert_eq!(plot.rendered, settings(800));
        // A render for a plot that was deleted meanwhile is handed back.
        assert_eq!(history.set_image("gone", "orphan", None, 4), Some("orphan"));

        history.mark_closed("plot-0");
        assert!(!history.current().unwrap().live);
        assert_eq!(history.clear().len(), 1);
        assert!(history.is_empty());
    }
}

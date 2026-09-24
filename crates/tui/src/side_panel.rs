//! Shared side-panel state (#431).
//!
//! One panel with switchable content instead of one more independent
//! overlay: whatever the panel shows, `^J` opens/focuses it and `Esc` closes
//! it — `Esc` does not change meaning with the content. Operations
//! (background jobs, later fleet operations) are the first content; the
//! fleet summary (#438) plugs in as another [`PanelContent`] variant.
//!
//! Layout rule: on a terminal at least [`SIDE_PANEL_MIN_TOTAL_WIDTH`] columns
//! wide the panel is permanent — docked next to the feed whenever it has
//! something to show, so jobs can be watched while working. Below the
//! threshold it would squeeze the feed, so it becomes a drawer that slides
//! out over the feed on `^J` and back on `Esc`.

use crate::ops::Operation;

/// Narrowest terminal (in columns) on which the panel docks permanently.
/// At 80 columns the feed keeps its full width and the panel is a drawer.
pub const SIDE_PANEL_MIN_TOTAL_WIDTH: u16 = 120;

/// Width of the docked panel.
pub const SIDE_PANEL_WIDTH: u16 = 40;

/// What the side panel currently shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelContent {
    /// Operations → hosts tree with the selected host's output tail.
    #[default]
    Operations,
}

/// One selectable row of the operations tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRow {
    /// An operation header (index into the operation list).
    Operation(usize),
    /// A host under an operation: `(operation, host)`.
    Host(usize, usize),
}

impl TreeRow {
    /// The `(operation, host)` whose output a selection on this row shows.
    /// An operation row shows its first host.
    pub fn target(self) -> (usize, usize) {
        match self {
            TreeRow::Operation(op) => (op, 0),
            TreeRow::Host(op, host) => (op, host),
        }
    }
}

/// Flatten operations into tree rows: each operation followed by its hosts.
pub fn tree_rows(ops: &[Operation]) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        rows.push(TreeRow::Operation(i));
        for h in 0..op.hosts.len() {
            rows.push(TreeRow::Host(i, h));
        }
    }
    rows
}

/// State of the shared side panel.
#[derive(Debug, Clone, Default)]
pub struct SidePanel {
    /// What the panel shows.
    pub content: PanelContent,
    /// Opened with `^J`: the panel has keyboard focus, and on a narrow
    /// terminal it is slid out over the feed.
    pub open: bool,
    /// Selected tree row.
    pub selected: usize,
}

impl SidePanel {
    /// `^J`: open (focus) the panel, or close it if already open.
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// `Esc`: close the panel. Returns whether anything changed, so the
    /// caller can let `Esc` fall through when the panel was not open.
    pub fn close(&mut self) -> bool {
        std::mem::replace(&mut self.open, false)
    }

    /// Move the selection by `delta` rows within `rows` rows.
    pub fn move_selection(&mut self, delta: isize, rows: usize) {
        if rows == 0 {
            self.selected = 0;
            return;
        }
        let max = rows as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }

    /// Keep the selection within `rows` after the content changed.
    pub fn clamp(&mut self, rows: usize) {
        self.selected = self.selected.min(rows.saturating_sub(1));
    }

    /// Whether the panel is drawn on a terminal `width` columns wide,
    /// given whether it has anything to show.
    pub fn visible(&self, width: u16, has_content: bool) -> bool {
        if docks(width) {
            self.open || has_content
        } else {
            self.open
        }
    }
}

/// Whether a terminal this wide docks the panel permanently.
pub fn docks(width: u16) -> bool {
    width >= SIDE_PANEL_MIN_TOTAL_WIDTH
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SessionId;
    use crate::ops::{HostOpState, OpHost};

    fn op(hosts: usize) -> Operation {
        Operation {
            session: SessionId(1),
            id: "job-1".into(),
            label: "x".into(),
            hosts: (0..hosts)
                .map(|i| OpHost {
                    name: format!("h{i}"),
                    state: HostOpState::Running,
                    exit_code: None,
                    tail: String::new(),
                    stale: false,
                    settled: true,
                })
                .collect(),
        }
    }

    #[test]
    fn tree_lists_each_operation_then_its_hosts() {
        let rows = tree_rows(&[op(1), op(2)]);
        assert_eq!(
            rows,
            vec![
                TreeRow::Operation(0),
                TreeRow::Host(0, 0),
                TreeRow::Operation(1),
                TreeRow::Host(1, 0),
                TreeRow::Host(1, 1),
            ]
        );
        assert_eq!(rows[2].target(), (1, 0));
        assert_eq!(rows[4].target(), (1, 1));
    }

    #[test]
    fn docked_on_wide_drawer_on_narrow() {
        let mut p = SidePanel::default();
        assert!(p.visible(160, true), "wide + content: permanent");
        assert!(!p.visible(160, false), "wide + nothing to show: no space taken");
        assert!(!p.visible(80, true), "80 columns: the feed keeps its width");
        p.toggle();
        assert!(p.visible(80, true), "^J slides the drawer out");
        assert!(p.visible(160, false), "^J shows the panel even when empty");
        assert!(p.close());
        assert!(!p.visible(80, true));
        assert!(!p.close(), "second Esc is not the panel's");
    }

    #[test]
    fn selection_is_clamped() {
        let mut p = SidePanel::default();
        p.move_selection(5, 3);
        assert_eq!(p.selected, 2);
        p.move_selection(-10, 3);
        assert_eq!(p.selected, 0);
        p.selected = 4;
        p.clamp(2);
        assert_eq!(p.selected, 1);
        p.clamp(0);
        assert_eq!(p.selected, 0);
    }
}

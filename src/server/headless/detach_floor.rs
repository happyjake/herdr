//! The configured headless size as a floor for tabs a small client leaves.

use super::*;

impl HeadlessServer {
    /// With no client left to size them, tabs keep the geometry the last
    /// client gave them. When that client was smaller than the configured
    /// headless size, grow them to it in whichever dimension fell short:
    /// repainting agents redraw only their current screen, so a small last
    /// client would otherwise cap every API read of their panes at its
    /// height until something attaches again.
    pub(super) fn raise_tabs_to_headless_size_after_last_detach(
        &mut self,
        detached_size: (u16, u16),
    ) {
        if self
            .clients
            .values()
            .any(|client| client.is_active_shell_client())
        {
            return;
        }
        let (floor_cols, floor_rows) = self.headless_size;
        let (cols, rows) = detached_size;
        if cols >= floor_cols && rows >= floor_rows {
            return;
        }
        let area = Rect::new(0, 0, cols.max(floor_cols), rows.max(floor_rows));
        for (workspace_index, workspace) in self.app.state.workspaces.iter().enumerate() {
            for tab_index in 0..workspace.tabs.len() {
                crate::ui::resize_tab_surface(
                    &self.app.state,
                    &self.app.terminal_runtimes,
                    workspace_index,
                    tab_index,
                    area,
                    crate::kitty_graphics::HostCellSize::default(),
                );
            }
        }
    }
}

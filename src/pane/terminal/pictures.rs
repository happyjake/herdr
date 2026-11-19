use super::*;
use crate::api::schema::{PanePicture, PanePictureKind, ReadSource};

#[derive(Default)]
pub(crate) struct PaneAnsiRead {
    pub text: String,
    pub truncated: bool,
    pub effective_offset: usize,
    pub has_more: bool,
    pub offset_unsupported: bool,
    pub pictures: Option<([u32; 2], Vec<PanePicture>)>,
}

/// A fetch may wait for a worker without extending the pane's lifetime.
pub(crate) struct PictureTerminal(std::sync::Weak<PaneTerminal>);

impl PictureTerminal {
    pub(in crate::pane) fn new(terminal: &Arc<PaneTerminal>) -> Self {
        Self(Arc::downgrade(terminal))
    }

    pub(crate) fn with_held_image<T>(
        &self,
        fingerprint: u64,
        read: impl FnOnce(crate::ghostty::KittyPictureImage<'_>) -> std::io::Result<T>,
    ) -> std::io::Result<Option<T>> {
        let Some(terminal) = self.0.upgrade() else {
            return Ok(None);
        };
        let core = terminal
            .ghostty
            .core
            .lock()
            .map_err(|_| std::io::Error::other("terminal lock poisoned"))?;
        core.terminal
            .kitty_picture_by_fingerprint(fingerprint)
            .map(read)
            .transpose()
    }
}

impl GhosttyPaneTerminal {
    pub(crate) fn read_pane_ansi(
        &self,
        source: ReadSource,
        lines: Option<usize>,
        offset: Option<usize>,
    ) -> PaneAnsiRead {
        let Ok(mut core) = self.core.lock() else {
            return PaneAnsiRead::default();
        };
        let unwrapped = source == ReadSource::RecentUnwrapped;
        let has_placements = core
            .terminal
            .kitty_graphics_may_have_placements()
            .unwrap_or(false);
        let (mut read, row_range) = if let Some(offset_from_bottom) = offset {
            let window = ghostty_recent_read_at_offset(
                &core,
                RecentReadRequest {
                    lines: lines.unwrap_or(80),
                    offset_from_bottom,
                    unwrapped,
                    ansi: true,
                },
            )
            .unwrap_or_default();
            (
                PaneAnsiRead {
                    text: window.text,
                    truncated: window.truncated,
                    effective_offset: window.effective_offset,
                    has_more: window.has_more,
                    offset_unsupported: window.offset_unsupported,
                    ..PaneAnsiRead::default()
                },
                window.row_range,
            )
        } else if source == ReadSource::Visible {
            let Ok(text) = ghostty_visible_ansi(&core) else {
                return PaneAnsiRead::default();
            };
            let row_range = has_placements
                .then(|| visible_picture_rows(&core.terminal, &text, lines))
                .flatten();
            let snapshot = crate::app::api_helpers::limit_snapshot_lines(text, lines);
            (
                PaneAnsiRead {
                    text: snapshot.text,
                    truncated: snapshot.truncated,
                    ..PaneAnsiRead::default()
                },
                row_range,
            )
        } else {
            let count = lines.unwrap_or(80);
            let Ok(range) = ghostty_recent_read_range(&core.terminal, count) else {
                return PaneAnsiRead::default();
            };
            let Ok(text) = read_ansi_range(&core.terminal, range, unwrapped) else {
                return PaneAnsiRead::default();
            };
            let native_empty = cfg!(windows) && has_placements && text.trim().is_empty();
            let snapshot = finish_recent_snapshot(&mut core, text, count, unwrapped);
            let row_range = if native_empty && !snapshot.text.trim().is_empty() {
                // The Windows console fallback has no addressable screen rows.
                None
            } else {
                range.map(|(start, end, _)| start as u32..=end as u32)
            };
            (
                PaneAnsiRead {
                    text: snapshot.text,
                    truncated: snapshot.truncated,
                    ..PaneAnsiRead::default()
                },
                row_range,
            )
        };

        if has_placements && !read.offset_unsupported {
            let row_range = if source != ReadSource::Visible
                && offset.unwrap_or(0) == 0
                && (row_range.is_some() || read.text.is_empty())
            {
                extend_recent_picture_rows(&core.terminal, row_range, lines.unwrap_or(80))
            } else {
                row_range
            };
            if let Some(rows) = row_range {
                read.pictures = read_pictures(&core.terminal, rows, unwrapped)
                    .map_err(|error| debug!(%error, "could not read Kitty picture placements"))
                    .ok()
                    .flatten();
            }
        }
        read
    }
}

fn visible_picture_rows(
    terminal: &crate::ghostty::Terminal,
    text: &str,
    lines: Option<usize>,
) -> Option<RangeInclusive<u32>> {
    let viewport = terminal.scrollbar().ok()?;
    let end = viewport.offset + viewport.len.checked_sub(1)?;
    // The line limit discards a prefix from the *formatted* rows. Keep
    // the viewport's trailing blank rows available for picture overlays.
    let skipped = lines.map_or(0, |n| text.split_inclusive('\n').count().saturating_sub(n));
    let start = viewport.offset + skipped;
    (lines != Some(0) && start <= end).then(|| start as u32..=end as u32)
}

fn extend_recent_picture_rows(
    terminal: &crate::ghostty::Terminal,
    rows: Option<RangeInclusive<u32>>,
    lines: usize,
) -> Option<RangeInclusive<u32>> {
    if lines == 0 {
        return None;
    }
    let total = terminal.total_rows().ok()? as u32;
    let bottom = total.checked_sub(1)?;
    let start = rows
        .as_ref()
        .map_or(total.saturating_sub(lines as u32), |r| *r.start());
    let end = rows.map_or(bottom, |r| {
        (*r.end())
            .max(start.saturating_add(lines as u32 - 1))
            .min(bottom)
    });
    Some(start..=end)
}

fn read_pictures(
    terminal: &crate::ghostty::Terminal,
    rows: RangeInclusive<u32>,
    unwrapped: bool,
) -> Result<Option<([u32; 2], Vec<PanePicture>)>, crate::ghostty::Error> {
    let start = *rows.start();
    let pictures = terminal.kitty_direct_pictures(rows, unwrapped)?;
    if pictures.is_empty() {
        return Ok(None);
    }
    // Map physical rows to logical line numbers in one forward pass, up
    // to the last placement start. A wrap tail at the window top is line 0.
    let mut row = start;
    let mut line = 0;
    let mut placements = Vec::with_capacity(pictures.len());
    for picture in pictures {
        let first = picture.screen_row.max(start);
        if unwrapped {
            while row < first {
                row += 1;
                if !terminal.row_wrap_continuation(row)? {
                    line += 1;
                }
            }
        } else {
            line = first - start;
        }
        placements.push(PanePicture {
            fingerprint: format!("{:016x}", picture.fingerprint),
            image_px: picture.image_px,
            line,
            rows_above: start.saturating_sub(picture.screen_row),
            col: picture.col,
            cols: picture.cols,
            rows: picture.rows,
            crop: [0, 0, picture.image_px[0], picture.image_px[1]],
            z: picture.z,
            opaque: picture.opaque,
            kind: PanePictureKind::Direct,
        });
    }
    placements.sort_by_key(|p| p.z);
    Ok(Some((terminal.cell_px()?, placements)))
}

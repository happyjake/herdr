use super::*;
use crate::api::schema::{EvictedPicture, PanePicture, PanePictureKind, PlacedPicture, ReadSource};

#[derive(Default)]
pub(crate) struct PaneAnsiRead {
    pub text: String,
    pub truncated: bool,
    pub effective_offset: usize,
    pub has_more: bool,
    pub offset_unsupported: bool,
    pub pictures: Option<PictureReadSamples>,
}

/// A fetch may wait for a worker without extending the pane's lifetime.
pub(crate) struct PictureTerminal(std::sync::Weak<PaneTerminal>);

impl PictureTerminal {
    pub(in crate::pane) fn new(terminal: &Arc<PaneTerminal>) -> Self {
        Self(Arc::downgrade(terminal))
    }

    pub(crate) fn held_image(
        &self,
        fingerprint: u64,
    ) -> std::io::Result<Option<crate::ghostty::KittyPictureImage>> {
        let Some(terminal) = self.0.upgrade() else {
            return Ok(None);
        };
        let core = terminal
            .ghostty
            .core
            .lock()
            .map_err(|_| std::io::Error::other("terminal lock poisoned"))?;
        let lookup = core.terminal.sample_kitty_picture_lookup(fingerprint)?;
        drop(core);
        lookup.finish()
    }
}

impl GhosttyPaneTerminal {
    pub(crate) fn read_pane_ansi(
        &self,
        source: ReadSource,
        lines: Option<usize>,
        offset: Option<usize>,
        revision: u64,
    ) -> PaneAnsiRead {
        let Ok(mut core) = self.core.lock() else {
            return PaneAnsiRead::default();
        };
        let unwrapped = source == ReadSource::RecentUnwrapped;
        let has_placements = core
            .terminal
            .kitty_graphics_may_have_picture_cells()
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
                && read.effective_offset == 0
                && (row_range.is_some() || read.text.is_empty())
            {
                extend_recent_picture_rows(&core.terminal, row_range, lines.unwrap_or(80))
            } else {
                row_range
            };
            if let Some(rows) = row_range {
                read.pictures = sample_read_pictures(
                    &core.terminal,
                    rows,
                    unwrapped,
                    offset.is_some_and(|offset| offset > 0),
                    revision,
                )
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
    (lines != Some(0) && start <= end).then_some(start as u32..=end as u32)
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

enum ReadPictureSample {
    Direct(crate::ghostty::KittyPictureSample),
    Virtual(crate::ghostty::KittyPictureSample),
    Evicted(crate::ghostty::KittyEvictedPictureSample),
}

pub(crate) struct PictureReadSamples {
    cell_px: [u32; 2],
    first_row: u32,
    pictures: Vec<ReadPictureSample>,
}

pub(crate) struct ReadPictures {
    pub cell_px: [u32; 2],
    pub entries: Vec<PanePicture>,
}

impl PictureReadSamples {
    pub(crate) fn finish(self) -> Option<ReadPictures> {
        let mut entries = Vec::with_capacity(self.pictures.len());
        for sample in self.pictures {
            let entry = match sample {
                ReadPictureSample::Direct(sample) => {
                    finish_placed_sample(sample, self.first_row, PanePictureKind::Direct)
                }
                ReadPictureSample::Virtual(sample) => {
                    finish_placed_sample(sample, self.first_row, PanePictureKind::Virtual)
                }
                ReadPictureSample::Evicted(sample) => Some(PanePicture::Evicted(EvictedPicture {
                    evicted: true,
                    line: sample.line,
                    rows_above: self.first_row.saturating_sub(sample.screen_row),
                    col: sample.col,
                    cols: sample.cols,
                    rows: sample.rows,
                })),
            };
            if let Some(entry) = entry {
                entries.push(entry);
            }
        }
        if entries.is_empty() {
            return None;
        }
        entries.sort_by_key(|entry| match entry {
            PanePicture::Placement(picture) => picture.z,
            PanePicture::Evicted(_) => 0,
        });
        Some(ReadPictures {
            cell_px: self.cell_px,
            entries,
        })
    }
}

fn finish_placed_sample(
    sample: crate::ghostty::KittyPictureSample,
    first_row: u32,
    kind: PanePictureKind,
) -> Option<PanePicture> {
    let picture = match sample.finish() {
        Ok(picture) => picture,
        Err(error) => {
            debug!(%error, "could not resolve a Kitty picture sample");
            return None;
        }
    };
    Some(PanePicture::Placement(PlacedPicture {
        fingerprint: format!("{:016x}", picture.fingerprint),
        image_px: picture.image_px,
        line: picture.line,
        rows_above: first_row.saturating_sub(picture.screen_row),
        col: picture.col,
        cols: picture.cols,
        rows: picture.rows,
        crop: picture.crop,
        z: picture.z,
        opaque: picture.opaque,
        kind,
    }))
}

fn sample_read_pictures(
    terminal: &crate::ghostty::Terminal,
    rows: RangeInclusive<u32>,
    unwrapped: bool,
    paged: bool,
    revision: u64,
) -> Result<Option<PictureReadSamples>, crate::ghostty::Error> {
    let first_row = *rows.start();
    let mut pictures: Vec<ReadPictureSample> = terminal
        .sample_kitty_direct_pictures(rows.clone(), unwrapped, paged, Some(revision))?
        .into_iter()
        .map(ReadPictureSample::Direct)
        .collect();
    pictures.extend(
        terminal
            .sample_kitty_virtual_pictures(rows, unwrapped, paged, Some(revision))?
            .into_iter()
            .map(|sample| match sample {
                crate::ghostty::KittyVirtualPictureSample::Stored(sample) => {
                    ReadPictureSample::Virtual(sample)
                }
                crate::ghostty::KittyVirtualPictureSample::Evicted(sample) => {
                    ReadPictureSample::Evicted(sample)
                }
            }),
    );
    if pictures.is_empty() {
        return Ok(None);
    }
    Ok(Some(PictureReadSamples {
        first_row,
        cell_px: terminal.cell_px()?,
        pictures,
    }))
}

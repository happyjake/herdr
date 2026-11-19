pub(crate) mod actor;
pub(crate) mod backend;
#[cfg(unix)]
pub(crate) mod fd;
pub(crate) mod output;

pub(crate) fn window_size(
    rows: u16,
    cols: u16,
    cell_width: u32,
    cell_height: u32,
) -> portable_pty::PtySize {
    let pixels =
        |cells: u16, px: u32| u32::from(cells).saturating_mul(px).min(u32::from(u16::MAX)) as u16;
    portable_pty::PtySize {
        rows,
        cols,
        pixel_width: pixels(cols, cell_width),
        pixel_height: pixels(rows, cell_height),
    }
}

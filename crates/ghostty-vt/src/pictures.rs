use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::ptr::NonNull;

use super::*;

#[derive(Debug)]
pub struct KittyPicture {
    pub fingerprint: u64,
    pub image_px: [u32; 2],
    pub screen_row: u32,
    pub line: u32,
    pub col: u16,
    pub cols: u32,
    pub rows: u32,
    pub crop: [u32; 4],
    pub z: i32,
    pub opaque: bool,
}

pub struct KittyPicturePixels<'a> {
    pub image_px: [u32; 2],
    pub format: KittyImageFormat,
    pub data: Cow<'a, [u8]>,
}

pub struct KittyPictureImage {
    image_px: [u32; 2],
    format: KittyImageFormat,
    bytes: RootBytes,
}

enum RootBytes {
    Inline(RootLease),
    File(Arc<OwnedExport>),
}

struct RootLease {
    raw: NonNull<c_void>,
    ptr: NonNull<u8>,
    len: usize,
    png: bool,
}

// The default Ghostty allocator is thread-safe. A retained allocation is
// immutable until its final reference is released, even after pane teardown.
unsafe impl Send for RootLease {}

impl Drop for RootLease {
    fn drop(&mut self) {
        unsafe { ghostty_kitty_graphics_image_root_unref(self.raw.as_ptr()) };
    }
}

impl KittyPictureImage {
    pub fn image_px(&self) -> [u32; 2] {
        self.image_px
    }

    pub fn pixels(&self) -> io::Result<KittyPicturePixels<'_>> {
        let (format, data) = match &self.bytes {
            RootBytes::File(source) => (KittyImageFormat::Rgba, Cow::Owned(source.copy_rgba()?)),
            RootBytes::Inline(root) => {
                let bytes = unsafe { slice::from_raw_parts(root.ptr.as_ptr(), root.len) };
                if root.png {
                    let decoded = decode_png_rgba(bytes)
                        .ok_or_else(|| io::Error::other("invalid Kitty root PNG"))?;
                    (KittyImageFormat::Rgba, Cow::Owned(decoded.data))
                } else {
                    (self.format, Cow::Borrowed(bytes))
                }
            }
        };
        Ok(KittyPicturePixels {
            image_px: self.image_px,
            format,
            data,
        })
    }
}

unsafe extern "C" {
    fn ghostty_kitty_graphics_storage_for_screen(
        terminal: ffi::GhosttyTerminal,
        alternate: bool,
    ) -> ffi::GhosttyKittyGraphics;
    fn ghostty_kitty_graphics_image_root_generation(image: ffi::GhosttyKittyGraphicsImage) -> u64;
    fn ghostty_kitty_graphics_image_root_ref(
        terminal: ffi::GhosttyTerminal,
        alternate: bool,
        id: u32,
        out_ref: *mut *mut c_void,
        out_ptr: *mut *const u8,
        out_len: *mut usize,
        out_png: *mut bool,
    ) -> ffi::GhosttyResult;
    fn ghostty_kitty_graphics_image_root_unref(root: *mut c_void);
    fn ghostty_kitty_graphics_visit_screen_pages(
        terminal: ffi::GhosttyTerminal,
        visit: extern "C" fn(*const c_void, u32, *mut c_void),
        context: *mut c_void,
    );
    fn ghostty_kitty_graphics_screen_row_evidence(
        terminal: ffi::GhosttyTerminal,
        first: u32,
        out: *mut u8,
        count: u32,
    ) -> ffi::GhosttyResult;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ImageIdentity {
    screen: ActiveScreen,
    id: u32,
    root_generation: u64,
}

#[derive(Clone, Copy)]
struct PictureContent {
    fingerprint: u64,
    opaque: bool,
}

#[derive(Default)]
pub(super) struct PictureCatalog {
    generations: [u64; 2],
    by_image: HashMap<ImageIdentity, Option<PictureContent>>,
    by_fingerprint: HashMap<u64, HashSet<ImageIdentity>>,
}

impl PictureCatalog {
    fn prune(&mut self, terminal: &Terminal) {
        let stores = [ActiveScreen::Primary, ActiveScreen::Alternate]
            .map(|screen| terminal.picture_storage(screen));
        let generations = stores.map(|graphics| {
            if graphics.is_null() {
                0
            } else {
                kitty_graphics_u64(
                    graphics,
                    ffi::GhosttyKittyGraphicsData_GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION,
                )
                .unwrap_or_default()
            }
        });
        if self.generations == generations {
            return;
        }
        self.generations = generations;
        self.by_image.retain(|id, _| {
            let graphics = stores[id.screen.index()];
            !graphics.is_null() && image_matches(graphics, *id).is_some()
        });
        self.by_fingerprint.retain(|_, candidates| {
            candidates.retain(|id| self.by_image.contains_key(id));
            !candidates.is_empty()
        });
    }
}

fn image_matches(
    storage: ffi::GhosttyKittyGraphics,
    id: ImageIdentity,
) -> Option<ffi::GhosttyKittyGraphicsImage> {
    if storage.is_null() {
        return None;
    }
    let image = unsafe { ffi::ghostty_kitty_graphics_image(storage, id.id) };
    (!image.is_null()
        && unsafe { ghostty_kitty_graphics_image_root_generation(image) } == id.root_generation)
        .then_some(image)
}

pub struct KittyPictureSample {
    placement: Placement,
    line: u32,
    image_px: [u32; 2],
    content: ContentSample,
}

enum ContentSample {
    Ready(PictureContent),
    Root {
        image: KittyPictureImage,
        id: ImageIdentity,
        catalog: Arc<Mutex<PictureCatalog>>,
    },
}

impl KittyPictureSample {
    /// Resolve metadata after releasing the terminal. The sample owns the
    /// root bytes from the same instant as the text and placement geometry.
    pub fn finish(self) -> io::Result<KittyPicture> {
        let content = match self.content {
            ContentSample::Ready(content) => content,
            ContentSample::Root { image, id, catalog } => {
                let cached = catalog
                    .lock()
                    .ok()
                    .and_then(|c| c.by_image.get(&id).copied().flatten());
                if let Some(content) = cached {
                    content
                } else {
                    let content = fingerprint_pixels(image.pixels()?)?;
                    if let Ok(mut catalog) = catalog.lock() {
                        if let Some(entry) = catalog.by_image.get_mut(&id) {
                            *entry = Some(content);
                            catalog
                                .by_fingerprint
                                .entry(content.fingerprint)
                                .or_default()
                                .insert(id);
                        }
                    }
                    content
                }
            }
        };
        let p = self.placement;
        Ok(KittyPicture {
            fingerprint: content.fingerprint,
            image_px: self.image_px,
            screen_row: p.screen_row,
            line: self.line,
            col: p.col,
            cols: p.cols,
            rows: p.rows,
            crop: p.crop,
            z: p.z,
            opaque: content.opaque,
        })
    }
}

fn fingerprint_pixels(pixels: KittyPicturePixels<'_>) -> io::Result<PictureContent> {
    let mut hasher = DefaultHasher::new();
    pixels.image_px.hash(&mut hasher);
    let opaque = match pixels.format {
        KittyImageFormat::Rgba => {
            hasher.write(&pixels.data);
            pixels.data.chunks_exact(4).all(|pixel| pixel[3] == 255)
        }
        KittyImageFormat::Rgb => {
            // Hash canonical RGBA so changing medium or expanding the
            // root to RGBA for animation preserves its content identity.
            let mut rgba = [255u8; 4096];
            for chunk in pixels.data.chunks(3072) {
                for (src, dst) in chunk.chunks_exact(3).zip(rgba.chunks_exact_mut(4)) {
                    dst[..3].copy_from_slice(src);
                }
                hasher.write(&rgba[..chunk.len() / 3 * 4]);
            }
            true
        }
        KittyImageFormat::Png => return Err(io::Error::other("expected decoded root pixels")),
    };
    Ok(PictureContent {
        fingerprint: hasher.finish(),
        opaque,
    })
}

#[derive(Clone, Copy)]
struct Placement {
    image_id: u32,
    screen_row: u32,
    bottom: u32,
    max_bottom: u32,
    col: u16,
    cols: u32,
    rows: u32,
    crop: [u32; 4],
    z: i32,
}

pub(super) struct PictureLayout {
    generation: u64,
    revision: Option<u64>,
    placements: Vec<Placement>,
    evidence: Option<RowEvidence>,
}

struct RowEvidence {
    range: RangeInclusive<u32>,
    wrapped: Vec<u32>,
    formatted: Vec<u32>,
    paged: Vec<u32>,
}

impl RowEvidence {
    fn new(terminal: &Terminal, range: RangeInclusive<u32>) -> Result<Self, Error> {
        const WRAP: u8 = 1;
        const CONTINUATION: u8 = 2;
        const FORMATTER_BREAK: u8 = 4;
        let count = range.end() - range.start() + 1;
        let mut flags = vec![0; count as usize];
        unsafe {
            ghostty_kitty_graphics_screen_row_evidence(
                terminal.raw,
                *range.start(),
                flags.as_mut_ptr(),
                count,
            )
            .into_result()?;
        }

        let mut wrapped = Vec::with_capacity(flags.len() + 1);
        let mut formatted = Vec::with_capacity(flags.len());
        let mut paged = Vec::with_capacity(flags.len());
        wrapped.push(0);
        let (mut wrap_count, mut line, mut page_line) = (0, 0, 0);
        for (index, &flag) in flags.iter().enumerate() {
            formatted.push(line);
            paged.push(page_line);
            wrap_count += u32::from(flag & (WRAP | CONTINUATION) != 0);
            wrapped.push(wrap_count);
            // PageFormatter adds a break after an empty row or a hard
            // break. Offset formatting also separates its logical groups.
            let formatter_break = flag & FORMATTER_BREAK != 0;
            line += u32::from(formatter_break);
            let group_break = flags
                .get(index + 1)
                .is_some_and(|next| next & CONTINUATION == 0);
            page_line += u32::from(formatter_break || group_break);
        }
        Ok(Self {
            range,
            wrapped,
            formatted,
            paged,
        })
    }

    fn contains(&self, rows: &RangeInclusive<u32>) -> bool {
        self.range.contains(rows.start()) && self.range.contains(rows.end())
    }

    fn is_wrapped(&self, placement: Placement) -> bool {
        let first = *self.range.start();
        self.wrapped[(placement.screen_row - first) as usize]
            != self.wrapped[(placement.bottom - first + 1) as usize]
    }

    fn line(&self, start: u32, row: u32, paged: bool) -> u32 {
        let lines = if paged { &self.paged } else { &self.formatted };
        let first = *self.range.start();
        lines[(row - first) as usize] - lines[(start - first) as usize]
    }
}

impl Terminal {
    pub fn cell_px(&self) -> Result<[u32; 2], Error> {
        Ok([
            self.width_px()? / u32::from(self.cols()?.max(1)),
            self.height_px()? / u32::from(self.rows()?.max(1)),
        ])
    }

    fn picture_storage(&self, screen: ActiveScreen) -> ffi::GhosttyKittyGraphics {
        unsafe {
            ghostty_kitty_graphics_storage_for_screen(self.raw, screen == ActiveScreen::Alternate)
        }
    }

    /// Resolve a previously issued fingerprint against the current image
    /// identity, including inactive screens and images without placements.
    pub fn kitty_picture_by_fingerprint(&self, fingerprint: u64) -> Option<KittyPictureImage> {
        let mut catalog = self.picture_catalog.lock().ok()?;
        catalog.prune(self);
        catalog
            .by_fingerprint
            .get(&fingerprint)?
            .iter()
            .find_map(|&id| {
                let image = image_matches(self.picture_storage(id.screen), id)?;
                let image_px = image_size(image).ok()?;
                self.retain_picture(id, image, image_px).ok()
            })
    }

    fn retain_picture(
        &self,
        id: ImageIdentity,
        image: ffi::GhosttyKittyGraphicsImage,
        image_px: [u32; 2],
    ) -> Result<KittyPictureImage, Error> {
        let format = kitty_image_format(image)?;
        let bytes = if let Some(source) = native_source::image_source(image)? {
            RootBytes::File(source)
        } else {
            let (mut raw, mut ptr, mut len, mut png) = (ptr::null_mut(), ptr::null(), 0, false);
            unsafe {
                ghostty_kitty_graphics_image_root_ref(
                    self.raw,
                    id.screen == ActiveScreen::Alternate,
                    id.id,
                    &mut raw,
                    &mut ptr,
                    &mut len,
                    &mut png,
                )
                .into_result()?;
            }
            // Success transfers one owned reference and initialized bytes.
            let raw = NonNull::new(raw).ok_or(Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE))?;
            let ptr = NonNull::new(ptr.cast_mut()).unwrap_or_else(NonNull::dangling);
            RootBytes::Inline(RootLease { raw, ptr, len, png })
        };
        Ok(KittyPictureImage {
            image_px,
            format,
            bytes,
        })
    }

    /// The revision covers writes and resizes that can move screen rows
    /// without changing the Kitty placement generation.
    pub fn sample_kitty_direct_pictures(
        &self,
        window: RangeInclusive<u32>,
        unwrapped: bool,
        paged: bool,
        revision: Option<u64>,
    ) -> Result<Vec<KittyPictureSample>, Error> {
        let graphics = self.kitty_graphics()?;
        if graphics.is_null() {
            return Ok(Vec::new());
        }
        let generation = kitty_graphics_u64(
            graphics,
            ffi::GhosttyKittyGraphicsData_GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION,
        )?;
        if generation == 0 || self.kitty_empty_generation.get() == Some(generation) {
            return Ok(Vec::new());
        }

        let mut cache = self.picture_layout.borrow_mut();
        if revision.is_none()
            || cache
                .as_ref()
                .is_none_or(|layout| layout.generation != generation || layout.revision != revision)
        {
            *cache = Some(PictureLayout {
                generation,
                revision,
                placements: self.picture_placements(graphics, generation)?,
                evidence: None,
            });
        }
        let Some(layout) = cache.as_mut() else {
            return Ok(Vec::new());
        };
        let start = *window.start();
        let end = *window.end();
        let first = layout.placements.partition_point(|p| p.max_bottom < start);
        let last = layout.placements.partition_point(|p| p.screen_row <= end);
        if first >= last {
            return Ok(Vec::new());
        }
        let candidates = &layout.placements[first..last];
        if unwrapped {
            let evidence_start = candidates[0].screen_row.min(start);
            let evidence_end = candidates[last - first - 1].max_bottom;
            let rows = evidence_start..=evidence_end;
            if layout.evidence.as_ref().is_none_or(|e| !e.contains(&rows)) {
                layout.evidence = Some(RowEvidence::new(self, rows)?);
            }
        }

        let screen = self.active_screen()?;
        let mut catalog = self
            .picture_catalog
            .lock()
            .map_err(|_| Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE))?;
        catalog.prune(self);
        let mut pictures = Vec::new();
        for &placement in candidates {
            if placement.bottom < start {
                continue;
            }
            let first_row = placement.screen_row.max(start);
            let line = if let Some(evidence) = layout.evidence.as_ref().filter(|_| unwrapped) {
                if evidence.is_wrapped(placement) {
                    continue;
                }
                evidence.line(start, first_row, paged)
            } else {
                first_row - start
            };
            match self.sample_picture(graphics, screen, placement, line, &mut catalog) {
                Ok(Some(sample)) => pictures.push(sample),
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "could not sample a Kitty picture placement"),
            }
        }
        Ok(pictures)
    }

    fn sample_picture(
        &self,
        graphics: ffi::GhosttyKittyGraphics,
        screen: ActiveScreen,
        placement: Placement,
        line: u32,
        catalog: &mut PictureCatalog,
    ) -> Result<Option<KittyPictureSample>, Error> {
        let image = unsafe { ffi::ghostty_kitty_graphics_image(graphics, placement.image_id) };
        if image.is_null() {
            return Ok(None);
        }
        let id = ImageIdentity {
            screen,
            id: placement.image_id,
            root_generation: unsafe { ghostty_kitty_graphics_image_root_generation(image) },
        };
        let image_px = image_size(image)?;
        let content = match *catalog.by_image.entry(id).or_default() {
            Some(content) => ContentSample::Ready(content),
            None => ContentSample::Root {
                image: self.retain_picture(id, image, image_px)?,
                id,
                catalog: Arc::clone(&self.picture_catalog),
            },
        };
        Ok(Some(KittyPictureSample {
            placement,
            line,
            image_px,
            content,
        }))
    }

    fn picture_placements(
        &self,
        graphics: ffi::GhosttyKittyGraphics,
        generation: u64,
    ) -> Result<Vec<Placement>, Error> {
        let mut raw = ptr::null_mut();
        unsafe {
            ffi::ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut raw)
                .into_result()?;
        }
        let _guard = KittyPlacementIteratorGuard { raw };
        let mut iterator = raw;
        unsafe {
            ffi::ghostty_kitty_graphics_get(
                graphics,
                ffi::GhosttyKittyGraphicsData_GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR,
                (&mut iterator as *mut ffi::GhosttyKittyGraphicsPlacementIterator).cast(),
            )
            .into_result()?;
        }
        let mut pictures = Vec::new();
        let mut has_placements = false;
        let mut pages = None;
        while unsafe { ffi::ghostty_kitty_graphics_placement_next(iterator) } {
            has_placements = true;
            match self.picture_placement(graphics, iterator, &mut pages) {
                Ok(Some(placement)) => pictures.push(placement),
                Ok(None) => {}
                Err(error) => tracing::debug!(%error, "could not locate a Kitty picture placement"),
            }
        }
        if !has_placements {
            self.kitty_empty_generation.set(Some(generation));
        }
        pictures.sort_by_key(|p| (p.screen_row, p.col, p.z, p.image_id));
        let mut max_bottom = 0;
        for picture in &mut pictures {
            max_bottom = max_bottom.max(picture.bottom);
            picture.max_bottom = max_bottom;
        }
        Ok(pictures)
    }

    fn picture_placement(
        &self,
        graphics: ffi::GhosttyKittyGraphics,
        iterator: ffi::GhosttyKittyGraphicsPlacementIterator,
        pages: &mut Option<HashMap<usize, u32>>,
    ) -> Result<Option<Placement>, Error> {
        if kitty_placement_bool(iterator, KITTY_PLACEMENT_DATA_IS_VIRTUAL)? {
            return Ok(None);
        }
        let image_id = kitty_placement_u32(
            iterator,
            ffi::GhosttyKittyGraphicsPlacementData_GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID,
        )?;
        let image = unsafe { ffi::ghostty_kitty_graphics_image(graphics, image_id) };
        if image.is_null() {
            return Ok(None);
        }
        let mut rect = ffi::GhosttySelection::default();
        let result = unsafe {
            ffi::ghostty_kitty_graphics_placement_rect(iterator, image, self.raw, &mut rect)
        };
        if result == ffi::GhosttyResult_GHOSTTY_NO_VALUE {
            return Ok(None);
        }
        result.into_result()?;

        let pages = pages.get_or_insert_with(|| self.picture_pages());
        let screen_row = |grid: &ffi::GhosttyGridRef| {
            pages
                .get(&(grid.node as usize))
                .map(|start| start + u32::from(grid.y))
                .ok_or(Error(ffi::GhosttyResult_GHOSTTY_NO_VALUE))
        };
        let (mut cols, mut rows) = (0, 0);
        let (mut x, mut y, mut w, mut h) = (0, 0, 0, 0);
        unsafe {
            ffi::ghostty_kitty_graphics_placement_grid_size(
                iterator, image, self.raw, &mut cols, &mut rows,
            )
            .into_result()?;
            ffi::ghostty_kitty_graphics_placement_source_rect(
                iterator, image, &mut x, &mut y, &mut w, &mut h,
            )
            .into_result()?;
        }
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let z = kitty_placement_i32(
            iterator,
            ffi::GhosttyKittyGraphicsPlacementData_GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z,
        )?;
        Ok(Some(Placement {
            image_id,
            screen_row: screen_row(&rect.start)?,
            bottom: screen_row(&rect.end)?,
            max_bottom: 0,
            col: rect.start.x,
            cols,
            rows,
            crop: [x, y, w, h],
            z,
        }))
    }

    fn picture_pages(&self) -> HashMap<usize, u32> {
        extern "C" fn page(node: *const c_void, row: u32, context: *mut c_void) {
            // The synchronous visitor does not retain the map or the node.
            unsafe { &mut *context.cast::<HashMap<usize, u32>>() }.insert(node as usize, row);
        }
        let mut pages = HashMap::<usize, u32>::new();
        unsafe {
            ghostty_kitty_graphics_visit_screen_pages(
                self.raw,
                page,
                (&mut pages as *mut HashMap<usize, u32>).cast(),
            );
        }
        pages
    }

    #[cfg(test)]
    pub fn kitty_direct_pictures(
        &self,
        window: RangeInclusive<u32>,
        unwrapped: bool,
    ) -> Result<Vec<KittyPicture>, Error> {
        Ok(self
            .sample_kitty_direct_pictures(window, unwrapped, false, None)?
            .into_iter()
            .filter_map(|sample| sample.finish().ok())
            .collect())
    }
}

fn image_size(image: ffi::GhosttyKittyGraphicsImage) -> Result<[u32; 2], Error> {
    Ok([
        kitty_image_u32(
            image,
            ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_WIDTH,
        )?,
        kitty_image_u32(
            image,
            ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_HEIGHT,
        )?,
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    #[test]
    fn picture_lookup_preserves_the_other_screens_desktop_fingerprint() {
        let mut terminal = Terminal::new(20, 10, 64 * 1024).unwrap();
        terminal.resize(20, 10, 8, 16).unwrap();
        terminal.enable_kitty_graphics().unwrap();

        // Primary screen: implicit-ID image (pi's a=T without i= -> id 2147483647).
        let primary_rgba = STANDARD.encode([10u8, 20, 30, 255].repeat(4));
        terminal.write(format!("\x1b_Ga=T,f=32,s=2,v=2,q=2;{primary_rgba}\x1b\\").as_bytes());
        let primary_pic = &terminal.kitty_direct_pictures(0..=9, false).unwrap()[0];
        let primary_fp = primary_pic.fingerprint;

        // Enter alternate screen: its ImageStorage independently starts implicit IDs
        // at 2147483647, colliding in Terminal::kitty_fingerprints (HashMap<u32, _>).
        let alt_rgba = STANDARD.encode([200u8, 100, 50, 255].repeat(4));
        terminal
            .write(format!("\x1b[?1049h\x1b_Ga=T,f=32,s=2,v=2,q=2;{alt_rgba}\x1b\\").as_bytes());
        let alt_placements = terminal.kitty_image_placements().unwrap();
        assert_eq!(alt_placements.len(), 1);
        let alt_id = alt_placements[0].image_id;
        let alt_fp = alt_placements[0].data_fingerprint;
        assert_ne!(primary_fp, alt_fp);
        assert_eq!(
            terminal.kitty_image_fingerprints(&[alt_id]).unwrap(),
            vec![Some(alt_fp)]
        );

        // A phone fetches the primary-screen picture (or misses on an evicted one)
        // while the pane is on the alternate screen.
        assert!(terminal.kitty_picture_by_fingerprint(primary_fp).is_some());

        // Expected: the active alternate-screen image was never deleted or replaced,
        // so kitty_image_fingerprints(&[alt_id]) must still return Some(alt_fp).
        assert_eq!(
            terminal.kitty_image_fingerprints(&[alt_id]).unwrap(),
            vec![Some(alt_fp)],
            "cross-screen visit_images clobbered active screen's cached fingerprint"
        );
    }

    #[test]
    fn picture_fingerprint_and_pixels_follow_the_root_frame() {
        let mut terminal = Terminal::new(20, 10, 0).unwrap();
        terminal.resize(20, 10, 8, 16).unwrap();
        terminal.enable_kitty_graphics().unwrap();
        terminal.write(b"\x1b_Ga=T,f=32,s=1,v=1,i=7,c=1,r=1,q=2,C=1;/wAA/w==\x1b\\");
        let root = terminal
            .kitty_direct_pictures(0..=9, false)
            .unwrap()
            .remove(0);
        let sampled = terminal
            .kitty_picture_by_fingerprint(root.fingerprint)
            .unwrap();

        // Display the second frame, whose pixels and alpha differ from the root.
        terminal
            .write(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=2;AAD/gA==\x1b\\\x1b_Ga=a,i=7,c=2,q=2;\x1b\\");
        let displayed = terminal.kitty_image_placements().unwrap();
        assert_ne!(displayed[0].data, [255, 0, 0, 255]);
        let while_playing = terminal
            .kitty_direct_pictures(0..=9, false)
            .unwrap()
            .remove(0);
        assert_eq!(while_playing.fingerprint, root.fingerprint);
        assert!(while_playing.opaque);
        assert_eq!(
            terminal
                .kitty_picture_by_fingerprint(root.fingerprint)
                .unwrap()
                .pixels()
                .unwrap()
                .data
                .as_ref(),
            &[255, 0, 0, 255],
        );

        // Mutating the root detaches it from a previously retained read.
        terminal.write(b"\x1b_Ga=f,i=7,r=1,f=32,s=1,v=1,X=1,q=2;AAD/gA==\x1b\\");
        let edited = terminal
            .kitty_direct_pictures(0..=9, false)
            .unwrap()
            .remove(0);
        assert_ne!(edited.fingerprint, root.fingerprint);
        assert!(!edited.opaque);
        assert!(terminal
            .kitty_picture_by_fingerprint(root.fingerprint)
            .is_none());
        drop(terminal);
        assert_eq!(sampled.pixels().unwrap().data.as_ref(), &[255, 0, 0, 255]);
    }
}

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::ptr::NonNull;

use super::*;

#[cfg(test)]
thread_local! {
    static DIRECT_LAYOUT_BUILDS: Cell<usize> = const { Cell::new(0) };
    static VIRTUAL_CELL_READS: Cell<usize> = const { Cell::new(0) };
    static VIRTUAL_STYLE_READS: Cell<usize> = const { Cell::new(0) };
    pub(super) static VIRTUAL_SPEC_BUILDS: Cell<usize> = const { Cell::new(0) };
}

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
    fn ghostty_kitty_graphics_visit_image_identities(
        storage: ffi::GhosttyKittyGraphics,
        visit: extern "C" fn(u32, u64, *mut c_void),
        context: *mut c_void,
    );
    fn ghostty_kitty_graphics_screen_wrap_ranges(
        terminal: ffi::GhosttyTerminal,
        ranges: *mut RowRange,
        count: usize,
    ) -> ffi::GhosttyResult;
    fn ghostty_kitty_graphics_placement_anchor(
        iterator: ffi::GhosttyKittyGraphicsPlacementIterator,
        anchor: *mut ffi::GhosttyGridRef,
    ) -> ffi::GhosttyResult;
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
    indexed_generations: [u64; 2],
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

    /// On a fetch miss, register any new identities without reading their bytes.
    fn index_stores(&mut self, terminal: &Terminal) {
        struct Visit<'a> {
            screen: ActiveScreen,
            images: &'a mut HashMap<ImageIdentity, Option<PictureContent>>,
        }
        extern "C" fn visit(id: u32, root_generation: u64, context: *mut c_void) {
            // The synchronous store visitor only passes this context during the call.
            let ctx = unsafe { &mut *context.cast::<Visit<'_>>() };
            ctx.images
                .entry(ImageIdentity {
                    screen: ctx.screen,
                    id,
                    root_generation,
                })
                .or_default();
        }

        for screen in [ActiveScreen::Primary, ActiveScreen::Alternate] {
            let i = screen.index();
            if self.indexed_generations[i] == self.generations[i] {
                continue;
            }
            let storage = terminal.picture_storage(screen);
            if !storage.is_null() {
                let mut ctx = Visit {
                    screen,
                    images: &mut self.by_image,
                };
                unsafe {
                    ghostty_kitty_graphics_visit_image_identities(
                        storage,
                        visit,
                        (&mut ctx as *mut Visit<'_>).cast(),
                    )
                };
            }
            self.indexed_generations[i] = self.generations[i];
        }
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

pub enum KittyVirtualPictureSample {
    Stored(KittyPictureSample),
    Evicted(KittyEvictedPictureSample),
}

pub struct KittyEvictedPictureSample {
    pub screen_row: u32,
    pub line: u32,
    pub col: u16,
    pub cols: u32,
    pub rows: u32,
}

pub struct KittyPictureSample {
    placement: Placement,
    line: u32,
    image_px: [u32; 2],
    content: ContentSample,
}

enum ContentSample {
    Ready(PictureContent),
    Root(PictureRoot),
}

struct PictureRoot {
    image: KittyPictureImage,
    id: ImageIdentity,
    catalog: Arc<Mutex<PictureCatalog>>,
}

impl PictureRoot {
    fn content(&self) -> io::Result<PictureContent> {
        if let Some(content) = self
            .catalog
            .lock()
            .ok()
            .and_then(|c| c.by_image.get(&self.id).copied().flatten())
        {
            return Ok(content);
        }
        let content = fingerprint_pixels(self.image.pixels()?)?;
        if let Ok(mut catalog) = self.catalog.lock() {
            if let Some(entry) = catalog.by_image.get_mut(&self.id) {
                *entry = Some(content);
                catalog
                    .by_fingerprint
                    .entry(content.fingerprint)
                    .or_default()
                    .insert(self.id);
            }
        }
        Ok(content)
    }
}

/// Immutable candidates from one storage snapshot. Resolve after releasing
/// the terminal lock so reading a new file or hashing pixels cannot stall PTY writes.
pub struct KittyPictureLookup {
    fingerprint: u64,
    indexed: Option<KittyPictureImage>,
    unindexed: Vec<PictureRoot>,
}

impl KittyPictureLookup {
    pub fn finish(self) -> io::Result<Option<KittyPictureImage>> {
        if let Some(image) = self.indexed {
            return Ok(Some(image));
        }
        let mut error = None;
        for root in self.unindexed {
            match root.content() {
                Ok(content) if content.fingerprint == self.fingerprint => {
                    return Ok(Some(root.image))
                }
                Ok(_) => {}
                Err(err) => error = Some(err),
            }
        }
        error.map_or(Ok(None), Err)
    }
}

impl KittyPictureSample {
    /// Resolve metadata after releasing the terminal. The sample owns the
    /// root bytes from the same instant as the text and placement geometry.
    pub fn finish(self) -> io::Result<KittyPicture> {
        let content = match self.content {
            ContentSample::Ready(content) => content,
            ContentSample::Root(root) => root.content()?,
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
    wrapped: Option<bool>,
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
    has_direct_placements: bool,
    placements: Vec<Placement>,
    evidence: Option<RowEvidence>,
}

#[repr(C)]
struct RowRange {
    first: u32,
    last: u32,
    clean_rows: u32,
}

pub(super) struct VirtualPictureLayout {
    screen: ActiveScreen,
    generation: u64,
    revision: Option<u64>,
    viewport_origin: usize,
    placements: Vec<VirtualPlacement>,
    evidence: Option<RowEvidence>,
}

struct RowEvidence {
    range: RangeInclusive<u32>,
    formatted: Vec<u32>,
    paged: Vec<u32>,
}

impl RowEvidence {
    fn new(terminal: &Terminal, range: RangeInclusive<u32>) -> Result<Self, Error> {
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

        let mut formatted = Vec::with_capacity(flags.len());
        let mut paged = Vec::with_capacity(flags.len());
        let (mut line, mut page_line) = (0, 0);
        for (index, &flag) in flags.iter().enumerate() {
            formatted.push(line);
            paged.push(page_line);
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
            formatted,
            paged,
        })
    }

    fn contains(&self, rows: &RangeInclusive<u32>) -> bool {
        self.range.contains(rows.start()) && self.range.contains(rows.end())
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
        self.sample_kitty_picture_lookup(fingerprint)
            .ok()?
            .finish()
            .ok()?
    }

    pub fn sample_kitty_picture_lookup(&self, fingerprint: u64) -> io::Result<KittyPictureLookup> {
        let mut catalog = self
            .picture_catalog
            .lock()
            .map_err(|_| io::Error::other("picture catalog poisoned"))?;
        catalog.prune(self);
        let retain = |id: ImageIdentity| -> Result<Option<KittyPictureImage>, Error> {
            let Some(image) = image_matches(self.picture_storage(id.screen), id) else {
                return Ok(None);
            };
            let size = image_size(image)?;
            match self.retain_picture(id, image, size) {
                Err(Error(ffi::GhosttyResult_GHOSTTY_NO_VALUE)) => Ok(None),
                other => other.map(Some),
            }
        };
        let mut lookup = KittyPictureLookup {
            fingerprint,
            indexed: None,
            unindexed: Vec::new(),
        };
        if let Some(candidates) = catalog.by_fingerprint.get(&fingerprint) {
            for &id in candidates {
                if let Some(image) = retain(id).map_err(io::Error::other)? {
                    lookup.indexed = Some(image);
                    return Ok(lookup);
                }
            }
        }
        catalog.index_stores(self);
        for (&id, content) in &catalog.by_image {
            if content.is_none() {
                if let Some(image) = retain(id).map_err(io::Error::other)? {
                    lookup.unindexed.push(PictureRoot {
                        id,
                        image,
                        catalog: Arc::clone(&self.picture_catalog),
                    });
                }
            }
        }
        Ok(lookup)
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
        if cache.as_ref().is_none_or(|layout| {
            layout.generation != generation
                || (layout.has_direct_placements
                    && (revision.is_none() || layout.revision != revision))
        }) {
            let (placements, has_direct_placements) =
                self.picture_placements(graphics, generation)?;
            *cache = Some(PictureLayout {
                generation,
                revision,
                has_direct_placements,
                placements,
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
        let candidates = &mut layout.placements[first..last];
        if unwrapped {
            let needs_wrap = |p: &Placement| p.bottom >= start && p.wrapped.is_none();
            let mut ranges: Vec<_> = candidates
                .iter()
                .filter(|p| needs_wrap(p))
                .map(|p| RowRange {
                    first: p.screen_row,
                    last: p.bottom,
                    clean_rows: 0,
                })
                .collect();
            if !ranges.is_empty() {
                unsafe {
                    ghostty_kitty_graphics_screen_wrap_ranges(
                        self.raw,
                        ranges.as_mut_ptr(),
                        ranges.len(),
                    )
                    .into_result()?;
                }
                for (p, range) in candidates.iter_mut().filter(|p| needs_wrap(p)).zip(ranges) {
                    p.wrapped = Some(range.clean_rows <= range.last - range.first);
                }
            }
            // Only surviving anchors need formatted line numbers. A placement
            // clipped at the window top is already known to be on line zero.
            if let Some(last) = candidates
                .iter()
                .rev()
                .find(|p| p.wrapped == Some(false) && p.screen_row > start)
            {
                let rows = start..=last.screen_row;
                if layout.evidence.as_ref().is_none_or(|e| !e.contains(&rows)) {
                    layout.evidence = Some(RowEvidence::new(self, rows)?);
                }
            }
        }

        let screen = self.active_screen()?;
        let mut catalog = self
            .picture_catalog
            .lock()
            .map_err(|_| Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE))?;
        catalog.prune(self);
        let mut pictures = Vec::new();
        for &placement in candidates.iter() {
            if placement.bottom < start || (unwrapped && placement.wrapped == Some(true)) {
                continue;
            }
            let first_row = placement.screen_row.max(start);
            let line = if let Some(evidence) = layout
                .evidence
                .as_ref()
                .filter(|_| unwrapped && start < first_row)
            {
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
            None => ContentSample::Root(PictureRoot {
                image: self.retain_picture(id, image, image_px)?,
                id,
                catalog: Arc::clone(&self.picture_catalog),
            }),
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
    ) -> Result<(Vec<Placement>, bool), Error> {
        #[cfg(test)]
        DIRECT_LAYOUT_BUILDS.with(|count| count.set(count.get() + 1));
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
        let mut has_direct_placements = false;
        let mut pages = None;
        let last_row = u32::try_from(self.total_rows()?.saturating_sub(1)).unwrap_or(u32::MAX);
        while unsafe { ffi::ghostty_kitty_graphics_placement_next(iterator) } {
            has_placements = true;
            match kitty_placement_bool(iterator, KITTY_PLACEMENT_DATA_IS_VIRTUAL) {
                Ok(true) => continue,
                Ok(false) => has_direct_placements = true,
                Err(error) => {
                    has_direct_placements = true;
                    tracing::debug!(%error, "could not identify a Kitty picture placement");
                    continue;
                }
            }
            match self.picture_placement(graphics, iterator, &mut pages, last_row) {
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
        Ok((pictures, has_direct_placements))
    }

    fn picture_placement(
        &self,
        graphics: ffi::GhosttyKittyGraphics,
        iterator: ffi::GhosttyKittyGraphicsPlacementIterator,
        pages: &mut Option<HashMap<usize, u32>>,
        last_row: u32,
    ) -> Result<Option<Placement>, Error> {
        let image_id = kitty_placement_u32(
            iterator,
            ffi::GhosttyKittyGraphicsPlacementData_GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID,
        )?;
        let image = unsafe { ffi::ghostty_kitty_graphics_image(graphics, image_id) };
        if image.is_null() {
            return Ok(None);
        }
        let mut anchor = ffi::GhosttyGridRef::default();
        let result = unsafe { ghostty_kitty_graphics_placement_anchor(iterator, &mut anchor) };
        if result == ffi::GhosttyResult_GHOSTTY_NO_VALUE {
            return Ok(None);
        }
        result.into_result()?;
        let pages = pages.get_or_insert_with(|| self.picture_pages());
        let page_start = pages
            .get(&(anchor.node as usize))
            .ok_or(Error(ffi::GhosttyResult_GHOSTTY_NO_VALUE))?;
        let screen_row = page_start + u32::from(anchor.y);
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
        if cols == 0 || rows == 0 || w == 0 || h == 0 {
            return Ok(None);
        }
        let z = kitty_placement_i32(
            iterator,
            ffi::GhosttyKittyGraphicsPlacementData_GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z,
        )?;
        Ok(Some(Placement {
            image_id,
            screen_row,
            bottom: screen_row.saturating_add(rows - 1).min(last_row),
            wrapped: None,
            max_bottom: 0,
            col: anchor.x,
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

#[derive(Clone, Copy)]
enum VirtualPlacement {
    Stored(Placement),
    Evicted(EvictedPlacement),
}

#[derive(Clone, Copy)]
struct EvictedPlacement {
    image_id: u32,
    screen_row: u32,
    bottom: u32,
    wrapped: Option<bool>,
    max_bottom: u32,
    col: u16,
    cols: u32,
    rows: u32,
}

impl VirtualPlacement {
    fn image_id(self) -> u32 {
        match self {
            Self::Stored(placement) => placement.image_id,
            Self::Evicted(placement) => placement.image_id,
        }
    }

    fn screen_row(self) -> u32 {
        match self {
            Self::Stored(placement) => placement.screen_row,
            Self::Evicted(placement) => placement.screen_row,
        }
    }

    fn bottom(self) -> u32 {
        match self {
            Self::Stored(placement) => placement.bottom,
            Self::Evicted(placement) => placement.bottom,
        }
    }

    fn wrapped(self) -> Option<bool> {
        match self {
            Self::Stored(placement) => placement.wrapped,
            Self::Evicted(placement) => placement.wrapped,
        }
    }

    fn set_wrapped(&mut self, wrapped: bool) {
        match self {
            Self::Stored(placement) => placement.wrapped = Some(wrapped),
            Self::Evicted(placement) => placement.wrapped = Some(wrapped),
        }
    }

    fn max_bottom(self) -> u32 {
        match self {
            Self::Stored(placement) => placement.max_bottom,
            Self::Evicted(placement) => placement.max_bottom,
        }
    }

    fn set_max_bottom(&mut self, max_bottom: u32) {
        match self {
            Self::Stored(placement) => placement.max_bottom = max_bottom,
            Self::Evicted(placement) => placement.max_bottom = max_bottom,
        }
    }

    fn col(self) -> u16 {
        match self {
            Self::Stored(placement) => placement.col,
            Self::Evicted(placement) => placement.col,
        }
    }

    fn z(self) -> i32 {
        match self {
            Self::Stored(placement) => placement.z,
            Self::Evicted(_) => 0,
        }
    }
}

impl Terminal {
    /// Placeholder geometry is cached by terminal revision as well as Kitty
    /// generation: writing or erasing placeholder cells does not mutate storage.
    pub fn sample_kitty_virtual_pictures(
        &self,
        window: RangeInclusive<u32>,
        unwrapped: bool,
        paged: bool,
        revision: Option<u64>,
    ) -> Result<Vec<KittyVirtualPictureSample>, Error> {
        let graphics = self.kitty_graphics()?;
        let generation = if graphics.is_null() {
            0
        } else {
            kitty_graphics_u64(
                graphics,
                ffi::GhosttyKittyGraphicsData_GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION,
            )?
        };
        let viewport = self.scrollbar()?;
        if viewport.offset > *window.end() as usize
            || *window.start() as usize >= viewport.offset + viewport.len
        {
            return Ok(Vec::new());
        }
        let screen = self.active_screen()?;

        let mut cache = self.virtual_picture_layout.borrow_mut();
        if revision.is_none()
            || cache.as_ref().is_none_or(|layout| {
                layout.screen != screen
                    || layout.generation != generation
                    || layout.revision != revision
                    || layout.viewport_origin != viewport.offset
            })
        {
            *cache = Some(VirtualPictureLayout {
                screen,
                generation,
                revision,
                viewport_origin: viewport.offset,
                placements: self
                    .virtual_picture_placements(graphics, screen, generation, viewport)?,
                evidence: None,
            });
        }
        let Some(layout) = cache.as_mut() else {
            return Ok(Vec::new());
        };
        let start = *window.start();
        let end = *window.end();
        let first = layout
            .placements
            .partition_point(|p| p.max_bottom() < start);
        let last = layout.placements.partition_point(|p| p.screen_row() <= end);
        if first >= last {
            return Ok(Vec::new());
        }
        let candidates = &mut layout.placements[first..last];
        if unwrapped {
            let needs_wrap = |p: &VirtualPlacement| p.bottom() >= start && p.wrapped().is_none();
            let mut ranges: Vec<_> = candidates
                .iter()
                .filter(|p| needs_wrap(p))
                .map(|p| RowRange {
                    first: p.screen_row(),
                    last: p.bottom(),
                    clean_rows: 0,
                })
                .collect();
            if !ranges.is_empty() {
                unsafe {
                    ghostty_kitty_graphics_screen_wrap_ranges(
                        self.raw,
                        ranges.as_mut_ptr(),
                        ranges.len(),
                    )
                    .into_result()?;
                }
                for (p, range) in candidates.iter_mut().filter(|p| needs_wrap(p)).zip(ranges) {
                    p.set_wrapped(range.clean_rows <= range.last - range.first);
                }
            }
            if let Some(last) = candidates
                .iter()
                .rev()
                .find(|p| p.wrapped() == Some(false) && p.screen_row() > start)
            {
                let rows = start..=last.screen_row();
                if layout.evidence.as_ref().is_none_or(|e| !e.contains(&rows)) {
                    layout.evidence = Some(RowEvidence::new(self, rows)?);
                }
            }
        }

        let mut catalog = self
            .picture_catalog
            .lock()
            .map_err(|_| Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE))?;
        catalog.prune(self);
        let mut pictures = Vec::new();
        for &placement in candidates.iter() {
            if placement.bottom() < start || (unwrapped && placement.wrapped() == Some(true)) {
                continue;
            }
            let first_row = placement.screen_row().max(start);
            let line = if let Some(evidence) = layout
                .evidence
                .as_ref()
                .filter(|_| unwrapped && start < first_row)
            {
                evidence.line(start, first_row, paged)
            } else {
                first_row - start
            };
            match placement {
                VirtualPlacement::Stored(placement) => {
                    match self.sample_picture(graphics, screen, placement, line, &mut catalog) {
                        Ok(Some(sample)) => {
                            pictures.push(KittyVirtualPictureSample::Stored(sample));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::debug!(%error, "could not sample a virtual Kitty picture");
                        }
                    }
                }
                VirtualPlacement::Evicted(placement) => {
                    pictures.push(KittyVirtualPictureSample::Evicted(
                        KittyEvictedPictureSample {
                            screen_row: placement.screen_row,
                            line,
                            col: placement.col,
                            cols: placement.cols,
                            rows: placement.rows,
                        },
                    ));
                }
            }
        }
        Ok(pictures)
    }

    fn viewport_row_has_virtual_placeholders(&self, y: u32) -> Result<bool, Error> {
        let grid_ref = self.grid_ref(ghostty_viewport_point(0, y))?;
        let mut row = 0;
        let mut marked = false;
        unsafe {
            ffi::ghostty_grid_ref_row(&grid_ref, &mut row).into_result()?;
            ffi::ghostty_row_get(
                row,
                ffi::GhosttyRowData_GHOSTTY_ROW_DATA_KITTY_VIRTUAL_PLACEHOLDER,
                (&mut marked as *mut bool).cast(),
            )
            .into_result()?;
        }
        Ok(marked)
    }

    pub(super) fn collect_viewport_virtual_runs(
        &self,
        y: u16,
        cols: u16,
        runs: &mut Vec<KittyVirtualRun>,
        graphemes: &mut Vec<u32>,
    ) {
        match self.viewport_row_has_virtual_placeholders(u32::from(y)) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::debug!(%error, y, "could not inspect a virtual placeholder row");
                return;
            }
        }

        let mut current: Option<KittyVirtualRun> = None;
        for x in 0..cols {
            let grid_ref = match self.grid_ref(ghostty_viewport_point(x, u32::from(y))) {
                Ok(grid_ref) => grid_ref,
                Err(error) => {
                    // Incomplete reflow can leave the rest of the row outside
                    // the page. Keep the runs already read and end this row.
                    tracing::debug!(%error, x, y, "could not locate a virtual placeholder cell");
                    break;
                }
            };
            match virtual_placeholder_cell(&grid_ref, x, y, graphemes) {
                Ok(Some(cell)) => {
                    if let Some(run) = current.as_mut() {
                        if run.append(cell) {
                            continue;
                        }
                        runs.push(*run);
                    }
                    current = Some(KittyVirtualRun::from_cell(cell));
                }
                result => {
                    if let Err(error) = result {
                        tracing::debug!(%error, x, y, "could not read a virtual placeholder cell");
                    }
                    runs.extend(current.take());
                }
            }
        }
        runs.extend(current);
    }

    fn virtual_picture_placements(
        &self,
        graphics: ffi::GhosttyKittyGraphics,
        screen: ActiveScreen,
        generation: u64,
        viewport: TerminalScrollbar,
    ) -> Result<Vec<VirtualPlacement>, Error> {
        let cols = self.cols()?;
        let rows = self.rows()?;
        let mut pictures = Vec::new();
        let mut active: Vec<ActiveVirtualPicture> = Vec::new();
        let mut next = Vec::new();
        let mut runs = Vec::new();
        let mut graphemes = Vec::new();
        let mut storage = None;

        for y in 0..rows {
            self.collect_viewport_virtual_runs(y, cols, &mut runs, &mut graphemes);
            let mut previous = active.drain(..).peekable();
            for run in runs.drain(..) {
                while let Some(picture) = previous.next_if(|picture| picture.x < run.x) {
                    pictures.extend(picture.finish());
                }

                let image_id = run.image_id();
                if image_id == 0 {
                    continue;
                }

                if let Some(mut picture) = previous.next_if(|picture| picture.x == run.x) {
                    if picture.appends(run) {
                        picture.append();
                        next.push(picture);
                        continue;
                    }
                    pictures.extend(picture.finish());
                }

                let (specs, cell_px) = match &storage {
                    Some(storage) => storage,
                    None => storage.insert((
                        self.virtual_placement_specs(graphics, screen, generation)?,
                        self.cell_px()?.map(|size| size.max(1)),
                    )),
                };

                let screen_row = viewport.offset as u32 + u32::from(run.y);
                let picture = match specs.get(image_id, run.placement_id()) {
                    Some(spec) => ActiveVirtualPicture::stored(run, spec, screen_row, *cell_px),
                    None if graphics.is_null()
                        || unsafe { ffi::ghostty_kitty_graphics_image(graphics, image_id) }
                            .is_null() =>
                    {
                        Some(ActiveVirtualPicture::evicted(run, screen_row))
                    }
                    None => None,
                };
                next.extend(picture);
            }
            pictures.extend(previous.filter_map(ActiveVirtualPicture::finish));
            mem::swap(&mut active, &mut next);
        }
        pictures.extend(active.into_iter().filter_map(ActiveVirtualPicture::finish));
        pictures.sort_by_key(|picture| {
            (
                picture.screen_row(),
                picture.col(),
                picture.z(),
                picture.image_id(),
            )
        });
        let mut max_bottom = 0;
        for picture in &mut pictures {
            max_bottom = max_bottom.max(picture.bottom());
            picture.set_max_bottom(max_bottom);
        }
        Ok(pictures)
    }
}

fn virtual_placeholder_cell(
    grid_ref: &ffi::GhosttyGridRef,
    x: u16,
    y: u16,
    graphemes: &mut Vec<u32>,
) -> Result<Option<KittyVirtualCell>, Error> {
    #[cfg(test)]
    VIRTUAL_CELL_READS.with(|count| count.set(count.get() + 1));
    let mut cell = ffi::GhosttyCell::default();
    let mut codepoint = 0;
    unsafe {
        ffi::ghostty_grid_ref_cell(grid_ref, &mut cell).into_result()?;
        ffi::ghostty_cell_get(
            cell,
            ffi::GhosttyCellData_GHOSTTY_CELL_DATA_CODEPOINT,
            (&mut codepoint as *mut u32).cast(),
        )
        .into_result()?;
    }
    if codepoint != KITTY_UNICODE_PLACEHOLDER {
        return Ok(None);
    }

    #[cfg(test)]
    VIRTUAL_STYLE_READS.with(|count| count.set(count.get() + 1));
    let mut style = ffi::GhosttyStyle {
        size: mem::size_of::<ffi::GhosttyStyle>(),
        ..Default::default()
    };
    unsafe { ffi::ghostty_grid_ref_style(grid_ref, &mut style).into_result()? };
    grid_ref_graphemes_into(grid_ref, graphemes)?;
    Ok(kitty_virtual_cell(x, y, graphemes, style.into()))
}

struct ActiveVirtualPicture {
    image_id: u32,
    placement_id: u32,
    x: u16,
    source_col: u32,
    width: u32,
    next_source_row: u32,
    value: ActiveVirtualPictureValue,
}

enum ActiveVirtualPictureValue {
    Stored {
        grid_rows: u32,
        crop_y: u32,
        crop_height: u32,
        placement: Placement,
    },
    Evicted(EvictedPlacement),
}

impl ActiveVirtualPicture {
    fn stored(
        run: KittyVirtualRun,
        spec: KittyVirtualPlacementSpec,
        screen_row: u32,
        [cell_width, cell_height]: [u32; 2],
    ) -> Option<Self> {
        let grid_cols = if spec.columns == 0 {
            spec.image_px[0].div_ceil(cell_width)
        } else {
            spec.columns
        };
        let grid_rows = if spec.rows == 0 {
            spec.image_px[1].div_ceil(cell_height)
        } else {
            spec.rows
        };
        let [crop_x, crop_y, crop_width, crop_height] = spec.crop;
        if run.col >= grid_cols || run.row >= grid_rows || crop_width == 0 || crop_height == 0 {
            return None;
        }
        let cols = run.width.min(grid_cols - run.col);
        let left = scale_u32(run.col, crop_width, grid_cols);
        let right = scale_u32(run.col + cols, crop_width, grid_cols);
        if left == right {
            return None;
        }
        let top = scale_u32(run.row, crop_height, grid_rows);
        let bottom = scale_u32(run.row + 1, crop_height, grid_rows);
        // A run can start with a fractional pixel row. Keep it active so a
        // later continuation can give the whole fragment a nonempty crop.
        Some(Self {
            image_id: run.image_id(),
            placement_id: run.placement_id(),
            x: run.x,
            source_col: run.col,
            width: run.width,
            next_source_row: run.row + 1,
            value: ActiveVirtualPictureValue::Stored {
                grid_rows,
                crop_y,
                crop_height,
                placement: Placement {
                    image_id: run.image_id(),
                    screen_row,
                    bottom: screen_row,
                    wrapped: None,
                    max_bottom: 0,
                    col: run.x,
                    cols,
                    rows: 1,
                    crop: [crop_x + left, crop_y + top, right - left, bottom - top],
                    z: spec.z,
                },
            },
        })
    }

    fn evicted(run: KittyVirtualRun, screen_row: u32) -> Self {
        Self {
            image_id: run.image_id(),
            placement_id: run.placement_id(),
            x: run.x,
            source_col: run.col,
            width: run.width,
            next_source_row: run.row + 1,
            value: ActiveVirtualPictureValue::Evicted(EvictedPlacement {
                image_id: run.image_id(),
                screen_row,
                bottom: screen_row,
                wrapped: None,
                max_bottom: 0,
                col: run.x,
                cols: run.width,
                rows: 1,
            }),
        }
    }

    fn appends(&self, run: KittyVirtualRun) -> bool {
        self.image_id == run.image_id()
            && self.placement_id == run.placement_id()
            && self.source_col == run.col
            && self.width == run.width
            && self.next_source_row == run.row
            && match self.value {
                ActiveVirtualPictureValue::Stored { grid_rows, .. } => run.row < grid_rows,
                ActiveVirtualPictureValue::Evicted(_) => true,
            }
    }

    fn append(&mut self) {
        self.next_source_row += 1;
        match &mut self.value {
            ActiveVirtualPictureValue::Stored {
                grid_rows,
                crop_y,
                crop_height,
                placement,
            } => {
                placement.rows += 1;
                placement.bottom += 1;
                let bottom = *crop_y + scale_u32(self.next_source_row, *crop_height, *grid_rows);
                placement.crop[3] = bottom - placement.crop[1];
            }
            ActiveVirtualPictureValue::Evicted(placement) => {
                placement.rows += 1;
                placement.bottom += 1;
            }
        }
    }

    fn finish(self) -> Option<VirtualPlacement> {
        match self.value {
            ActiveVirtualPictureValue::Stored { placement, .. } => {
                (placement.crop[3] > 0).then_some(VirtualPlacement::Stored(placement))
            }
            ActiveVirtualPictureValue::Evicted(placement) => {
                Some(VirtualPlacement::Evicted(placement))
            }
        }
    }
}

pub(super) fn image_size(image: ffi::GhosttyKittyGraphicsImage) -> Result<[u32; 2], Error> {
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

    fn terminal(cols: u16, rows: u16) -> Terminal {
        let mut terminal = Terminal::new(cols, rows, 1_000_000).unwrap();
        terminal.resize(cols, rows, 8, 16).unwrap();
        terminal.enable_kitty_graphics().unwrap();
        terminal
    }

    fn transmit_virtual(terminal: &mut Terminal, image_id: u32, cols: u32, rows: u32) {
        let rgba = STANDARD.encode([80u8, 90, 100, 255].repeat((cols * rows) as usize));
        terminal.write(
            format!(
                "\x1b_Ga=T,U=1,f=32,s={cols},v={rows},i={image_id},c={cols},r={rows},q=2;{rgba}\x1b\\"
            )
            .as_bytes(),
        );
    }

    fn placeholder(terminal: &mut Terminal, x: u16, y: u16, image_id: u32, row: u16, cols: u16) {
        const ROWS: [char; 10] = [
            '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}',
            '\u{033f}', '\u{0346}', '\u{034a}',
        ];
        let [blue, green, red] = image_id.to_le_bytes()[..3] else {
            unreachable!()
        };
        terminal
            .write(format!("\x1b[{};{}H\x1b[38;2;{red};{green};{blue}m", y + 1, x + 1).as_bytes());
        for _ in 0..cols {
            terminal.write("\u{10eeee}".as_bytes());
        }
        // Rewrite the row leader with its diacritics after establishing the run.
        terminal.write(
            format!(
                "\x1b[{};{}H\u{10eeee}{}{}",
                y + 1,
                x + 1,
                ROWS[row as usize],
                ROWS[0]
            )
            .as_bytes(),
        );
    }

    fn sample_virtual(
        terminal: &Terminal,
        unwrapped: bool,
        revision: Option<u64>,
    ) -> Vec<KittyVirtualPictureSample> {
        let last = terminal.total_rows().unwrap().saturating_sub(1) as u32;
        terminal
            .sample_kitty_virtual_pictures(0..=last, unwrapped, false, revision)
            .unwrap()
    }

    #[test]
    fn virtual_unwrapped_handles_different_depths_on_the_same_row() {
        let mut terminal = terminal(100, 20);
        transmit_virtual(&mut terminal, 101, 2, 8);
        transmit_virtual(&mut terminal, 102, 2, 2);
        for row in 0..8 {
            placeholder(&mut terminal, 0, row + 2, 101, row, 2);
        }
        for row in 0..2 {
            placeholder(&mut terminal, 50, row + 2, 102, row, 2);
        }

        let pictures = sample_virtual(&terminal, true, None);
        assert_eq!(pictures.len(), 2);
        for (sample, rows) in pictures.into_iter().zip([8, 2]) {
            let KittyVirtualPictureSample::Stored(sample) = sample else {
                panic!("expected a stored picture");
            };
            assert_eq!(sample.placement.screen_row, 2);
            assert_eq!(sample.placement.rows, rows);
        }
    }

    #[test]
    fn picture_free_viewport_does_not_decode_cells() {
        let mut terminal = terminal(180, 60);
        terminal.write(b"ordinary text only");
        VIRTUAL_CELL_READS.set(0);

        let pictures = sample_virtual(&terminal, false, Some(1));

        assert!(pictures.is_empty());
        assert_eq!(VIRTUAL_CELL_READS.get(), 0);
    }

    #[test]
    fn virtual_scan_decodes_style_only_for_placeholder_cells() {
        let mut terminal = terminal(180, 60);
        transmit_virtual(&mut terminal, 105, 2, 1);
        terminal.write(b"\x1b[2;1Hordinary text on the same row");
        placeholder(&mut terminal, 40, 1, 105, 0, 2);
        VIRTUAL_CELL_READS.set(0);
        VIRTUAL_STYLE_READS.set(0);

        assert_eq!(sample_virtual(&terminal, false, Some(1)).len(), 1);
        assert_eq!(VIRTUAL_CELL_READS.get(), 180);
        assert_eq!(VIRTUAL_STYLE_READS.get(), 2);
    }

    #[test]
    fn virtual_specs_survive_text_only_revisions() {
        let mut terminal = terminal(20, 10);
        for image_id in 104..204 {
            transmit_virtual(&mut terminal, image_id, 1, 1);
        }
        placeholder(&mut terminal, 0, 0, 104, 0, 1);
        VIRTUAL_SPEC_BUILDS.set(0);
        assert_eq!(sample_virtual(&terminal, false, Some(1)).len(), 1);

        terminal.write(b"\x1b[10;1Hunrelated text");
        assert_eq!(sample_virtual(&terminal, false, Some(2)).len(), 1);
        assert_eq!(VIRTUAL_SPEC_BUILDS.get(), 1);
    }

    #[test]
    fn virtual_scan_keeps_readable_runs_at_the_end_of_a_short_row() {
        let mut terminal = terminal(20, 10);
        placeholder(&mut terminal, 17, 0, 105, 0, 2);
        let mut runs = Vec::new();

        // Ask for a column past the native row width, as can happen on a
        // page that is still narrow after incomplete reflow.
        terminal.collect_viewport_virtual_runs(0, 21, &mut runs, &mut Vec::new());

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].image_id(), 105);
        assert_eq!(runs[0].width, 2);
    }

    #[test]
    fn virtual_only_picture_storage_skips_direct_layout_rebuilds() {
        let mut terminal = terminal(20, 10);
        transmit_virtual(&mut terminal, 104, 1, 1);
        DIRECT_LAYOUT_BUILDS.set(0);
        assert!(terminal
            .sample_kitty_direct_pictures(0..=9, false, false, Some(1))
            .unwrap()
            .is_empty());

        terminal.write(b"unrelated text");
        assert!(terminal
            .sample_kitty_direct_pictures(0..=9, false, false, Some(2))
            .unwrap()
            .is_empty());
        assert_eq!(DIRECT_LAYOUT_BUILDS.get(), 1);

        terminal.write(b"\x1b_Ga=T,f=32,s=1,v=1,i=7,c=1,r=1,q=2,C=1;/wAA/w==\x1b\\");
        assert_eq!(
            terminal
                .sample_kitty_direct_pictures(0..=9, false, false, Some(3))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(DIRECT_LAYOUT_BUILDS.get(), 2);
    }

    #[test]
    fn virtual_layout_cache_tracks_the_viewport_origin() {
        let mut terminal = terminal(40, 10);
        transmit_virtual(&mut terminal, 103, 2, 2);
        for row in 0..2 {
            placeholder(&mut terminal, 2, row, 103, row, 2);
        }
        terminal.write(b"\x1b[10;1H\x1b[0m");
        for line in 0..30 {
            terminal.write(format!("history {line}\r\n").as_bytes());
        }
        assert!(sample_virtual(&terminal, false, Some(7)).is_empty());

        terminal.scroll_viewport_row(0);
        let pictures = sample_virtual(&terminal, false, Some(7));
        assert_eq!(pictures.len(), 1);
        let KittyVirtualPictureSample::Stored(picture) = &pictures[0] else {
            panic!("expected a stored picture");
        };
        assert_eq!(picture.placement.screen_row, 0);
        assert_eq!(picture.placement.rows, 2);

        terminal.scroll_viewport_bottom();
        assert!(sample_virtual(&terminal, false, Some(7)).is_empty());
    }

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

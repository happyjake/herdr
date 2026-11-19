use super::*;

#[derive(Debug)]
pub struct KittyPicture {
    pub fingerprint: u64,
    pub image_px: [u32; 2],
    pub screen_row: u32,
    pub col: u16,
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    pub opaque: bool,
}

pub struct KittyPicturePixels {
    pub image_px: [u32; 2],
    pub format: KittyImageFormat,
    pub data: Vec<u8>,
}

pub struct KittyPictureImage<'a> {
    image: ffi::GhosttyKittyGraphicsImage,
    descriptor: KittyImageDescriptor,
    format: KittyImageFormat,
    source: Option<Arc<OwnedExport>>,
    _terminal: PhantomData<&'a Terminal>,
}

impl KittyPictureImage<'_> {
    pub fn image_px(&self) -> [u32; 2] {
        [self.descriptor.image_width, self.descriptor.image_height]
    }

    pub fn pixels(&self) -> Result<KittyPicturePixels, Error> {
        let data = if let Some(source) = &self.source {
            source
                .copy_rgba()
                .map_err(|_| Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE))?
        } else {
            let (ptr, len) = kitty_image_data_ptr_len(self.image)?;
            kitty_image_data_from_ptr(ptr, len)
        };
        Ok(KittyPicturePixels {
            image_px: self.image_px(),
            format: self.format,
            data,
        })
    }
}

unsafe extern "C" {
    fn herdr_kitty_graphics_visit_images(
        terminal: ffi::GhosttyTerminal,
        visit: extern "C" fn(ffi::GhosttyKittyGraphicsImage, *mut c_void) -> bool,
        context: *mut c_void,
    );
}

struct PictureSearch<'a> {
    terminal: &'a Terminal,
    fingerprint: u64,
    found: Option<KittyPictureImage<'a>>,
}

extern "C" fn visit_image(image: ffi::GhosttyKittyGraphicsImage, context: *mut c_void) -> bool {
    // The vendor visitor calls this synchronously with a borrow of a live
    // image. The context is private to this search, never retained by Zig.
    let search = unsafe { &mut *context.cast::<PictureSearch<'_>>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let image_id = kitty_image_u32(
            image,
            ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_ID,
        )?;
        let image = search.terminal.kitty_picture_image(image, image_id)?;
        Ok::<_, Error>((image.descriptor.data_fingerprint == search.fingerprint).then_some(image))
    }));
    match result {
        Ok(Ok(found)) => {
            search.found = found;
            search.found.is_some()
        }
        Ok(Err(_)) => false,
        Err(_) => true,
    }
}

impl Terminal {
    /// The caller must keep the terminal borrowed until it has copied pixels.
    /// Look in both screens, including images whose placements were removed.
    pub fn kitty_picture_by_fingerprint(&self, fingerprint: u64) -> Option<KittyPictureImage<'_>> {
        let mut search = PictureSearch {
            terminal: self,
            fingerprint,
            found: None,
        };
        unsafe {
            herdr_kitty_graphics_visit_images(
                self.raw,
                visit_image,
                (&mut search as *mut PictureSearch<'_>).cast(),
            )
        };
        search.found
    }

    fn kitty_picture_image(
        &self,
        image: ffi::GhosttyKittyGraphicsImage,
        image_id: u32,
    ) -> Result<KittyPictureImage<'_>, Error> {
        let width = kitty_image_u32(
            image,
            ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_WIDTH,
        )?;
        let height = kitty_image_u32(
            image,
            ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_HEIGHT,
        )?;
        let format = kitty_image_format(image)?;
        let (descriptor, _, source) =
            self.kitty_placement_payload(image, image_id, 0, width, height, format, &mut |_| {
                false
            })?;
        Ok(KittyPictureImage {
            image,
            descriptor,
            format,
            source,
            _terminal: PhantomData,
        })
    }

    pub fn cell_px(&self) -> Result<[u32; 2], Error> {
        Ok([
            self.width_px()? / u32::from(self.cols()?.max(1)),
            self.height_px()? / u32::from(self.rows()?.max(1)),
        ])
    }

    /// An ANSI read addresses the screen and its scrollback in one coordinate
    /// space. Query the placement pin even when the viewport cannot see it.
    pub fn kitty_direct_pictures(
        &self,
        window: RangeInclusive<u32>,
        unwrapped: bool,
    ) -> Result<Vec<KittyPicture>, Error> {
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
        while unsafe { ffi::ghostty_kitty_graphics_placement_next(iterator) } {
            has_placements = true;
            if let Some(picture) =
                self.kitty_direct_picture(graphics, iterator, &window, unwrapped)?
            {
                pictures.push(picture);
            }
        }
        if !has_placements {
            self.kitty_empty_generation.set(Some(generation));
        }
        self.prune_kitty_fingerprints(graphics);
        pictures.sort_by_key(|p| (p.screen_row, p.col, p.z, p.fingerprint));
        Ok(pictures)
    }

    fn kitty_direct_picture(
        &self,
        graphics: ffi::GhosttyKittyGraphics,
        iterator: ffi::GhosttyKittyGraphicsPlacementIterator,
        window: &RangeInclusive<u32>,
        unwrapped: bool,
    ) -> Result<Option<KittyPicture>, Error> {
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
        // A resolved crop is independent of the pin and grid span.
        let mut rect = ffi::GhosttySelection::default();
        let result = unsafe {
            ffi::ghostty_kitty_graphics_placement_rect(iterator, image, self.raw, &mut rect)
        };
        if result == ffi::GhosttyResult_GHOSTTY_NO_VALUE {
            return Ok(None);
        }
        result.into_result()?;
        let origin = self.kitty_screen_point(&rect.start)?;
        if origin.y > *window.end() {
            return Ok(None);
        }
        let end = self.kitty_screen_point(&rect.end)?;
        if end.y < *window.start() {
            return Ok(None);
        }
        if unwrapped {
            for row in origin.y..=end.y {
                let grid_ref = self.grid_ref(ghostty_screen_point(0, row))?;
                let (wrapped, continuation) = grid_ref_wrap_state(&grid_ref)?;
                if wrapped || continuation {
                    return Ok(None);
                }
            }
        }

        let (mut cols, mut rows) = (0, 0);
        unsafe {
            ffi::ghostty_kitty_graphics_placement_grid_size(
                iterator, image, self.raw, &mut cols, &mut rows,
            )
            .into_result()?;
        }
        let picture_image = self.kitty_picture_image(image, image_id)?;
        let opaque = self.kitty_picture_opaque(
            image,
            picture_image.descriptor,
            picture_image.format,
            picture_image.source.as_deref(),
        )?;
        let z = kitty_placement_i32(
            iterator,
            ffi::GhosttyKittyGraphicsPlacementData_GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z,
        )?;
        Ok(Some(KittyPicture {
            fingerprint: picture_image.descriptor.data_fingerprint,
            image_px: picture_image.image_px(),
            screen_row: origin.y,
            col: origin.x,
            cols,
            rows,
            z,
            opaque,
        }))
    }

    fn kitty_screen_point(
        &self,
        grid_ref: &ffi::GhosttyGridRef,
    ) -> Result<ffi::GhosttyPointCoordinate, Error> {
        let mut out = ffi::GhosttyPointCoordinate::default();
        unsafe {
            ffi::ghostty_terminal_point_from_grid_ref(
                self.raw,
                grid_ref,
                ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
                &mut out,
            )
            .into_result()?;
        }
        Ok(out)
    }

    fn kitty_picture_opaque(
        &self,
        image: ffi::GhosttyKittyGraphicsImage,
        descriptor: KittyImageDescriptor,
        format: KittyImageFormat,
        source: Option<&OwnedExport>,
    ) -> Result<bool, Error> {
        if format == KittyImageFormat::Rgb {
            return Ok(true);
        }

        let generation = kitty_image_u64(
            image,
            ffi::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_GENERATION,
        )?;
        let fingerprint = descriptor.data_fingerprint;
        let cached = self.kitty_fingerprints.lock().ok().and_then(|cache| {
            cache
                .get(&descriptor.image_id)
                .filter(|entry| entry.generation == generation && entry.fingerprint == fingerprint)
                .and_then(|entry| entry.opaque)
        });
        if let Some(opaque) = cached {
            return Ok(opaque);
        }

        let is_opaque = |data: &[u8]| data.chunks_exact(4).all(|pixel| pixel[3] == 255);
        let opaque = if let Some(source) = source {
            let pixels = source
                .copy_rgba()
                .map_err(|_| Error(ffi::GhosttyResult_GHOSTTY_INVALID_VALUE))?;
            is_opaque(&pixels)
        } else {
            let (ptr, len) = kitty_image_data_ptr_len(image)?;
            !ptr.is_null() && len > 0 && is_opaque(unsafe { slice::from_raw_parts(ptr, len) })
        };
        if let Ok(mut cache) = self.kitty_fingerprints.lock() {
            cache.insert(
                descriptor.image_id,
                KittyImageFingerprintEntry {
                    generation,
                    fingerprint,
                    opaque: Some(opaque),
                },
            );
        }
        Ok(opaque)
    }
}

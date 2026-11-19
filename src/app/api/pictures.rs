use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use tokio::sync::Semaphore;

use crate::api::schema::{PanePictureMediaType, PanePictureParams, ResponseResult};
use crate::ghostty::{KittyImageFormat, KittyPicturePixels};
use crate::pane::PictureTerminal;

use super::responses::{encode_error, encode_success};

const MAX_ENCODED_BYTES: usize = 768 * 1024;
const MAX_EDGE_PX: u32 = 4096;
const CACHE_BYTES: usize = 16 * 1024 * 1024;

static ENCODE_JOBS: Semaphore = Semaphore::const_new(2);
static CACHE: LazyLock<Mutex<EncodeCache>> = LazyLock::new(Mutex::default);

type CacheKey = (u64, u32);

#[derive(Default)]
struct EncodeCache {
    entries: HashMap<CacheKey, Arc<EncodedPicture>>,
    order: VecDeque<CacheKey>,
    bytes: usize,
}

impl EncodeCache {
    fn insert(&mut self, key: CacheKey, picture: Arc<EncodedPicture>) {
        if self.entries.contains_key(&key) {
            return;
        }
        let bytes = picture.data_b64.len();
        while self.bytes + bytes > CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                return;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.bytes -= evicted.data_b64.len();
            }
        }
        self.bytes += bytes;
        self.order.push_back(key);
        self.entries.insert(key, picture);
    }
}

struct EncodedPicture {
    px: [u32; 2],
    media_type: PanePictureMediaType,
    data_b64: String,
}

pub(super) fn fetch(
    terminal: PictureTerminal,
    id: String,
    params: PanePictureParams,
    respond_to: mpsc::Sender<String>,
) {
    let Some(fingerprint) = parse_fingerprint(&params.fingerprint) else {
        let _ = respond_to.send(picture_not_held(id));
        return;
    };
    let width_bucket = params.width_px.clamp(64, 2048).div_ceil(64) * 64;
    let Ok(permit) = ENCODE_JOBS.try_acquire() else {
        let _ = respond_to.send(encode_error(id, "picture_busy", "picture workers are busy"));
        return;
    };
    tokio::spawn(async move {
        let reply_id = id.clone();
        let response = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match fetch_picture(terminal, fingerprint, width_bucket) {
                Ok(Some(picture)) => encode_success(
                    id,
                    ResponseResult::PanePicture {
                        fingerprint: params.fingerprint,
                        media_type: picture.media_type,
                        px: picture.px,
                        data_b64: picture.data_b64.clone(),
                    },
                ),
                Ok(None) => picture_not_held(id),
                Err(error) => {
                    tracing::debug!(%error, "could not encode Kitty picture");
                    encode_error(
                        id,
                        "picture_encode_failed",
                        "could not encode the stored picture",
                    )
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            encode_error(
                reply_id,
                "picture_encode_failed",
                "picture encoding did not complete",
            )
        });
        let _ = respond_to.send(response);
    });
}

fn parse_fingerprint(value: &str) -> Option<u64> {
    if value.len() != 16 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}

fn picture_not_held(id: String) -> String {
    encode_error(
        id,
        "picture_not_held",
        "the pane does not hold this picture",
    )
}

fn fetch_picture(
    terminal: PictureTerminal,
    fingerprint: u64,
    width_bucket: u32,
) -> io::Result<Option<Arc<EncodedPicture>>> {
    let Some(image) = terminal.held_image(fingerprint)? else {
        return Ok(None);
    };
    // Validate live storage before the process-wide cache. Widths above
    // the source's native width share one actual encode.
    let width = width_bucket.min(image.image_px()[0]);
    let key = (fingerprint, width);
    if let Some(picture) = CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.entries.get(&key).cloned())
    {
        return Ok(Some(picture));
    }

    #[cfg(debug_assertions)]
    wait_at_test_encode_barrier()?;
    let picture = Arc::new(encode_picture(image.pixels()?, width)?);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(key, picture.clone());
    }
    Ok(Some(picture))
}

#[cfg(debug_assertions)]
fn wait_at_test_encode_barrier() -> io::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static ARRIVALS: AtomicUsize = AtomicUsize::new(0);
    let Some(path) = std::env::var_os("HERDR_TEST_PICTURE_ENCODE_BARRIER") else {
        return Ok(());
    };
    let dir = std::path::PathBuf::from(path);
    let arrival = ARRIVALS.fetch_add(1, Ordering::Relaxed);
    std::fs::write(dir.join(format!("entered-{arrival}")), [])?;
    let release = dir.join("release");
    let deadline = Instant::now() + Duration::from_secs(8);
    while !release.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("picture encode barrier timed out"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn encode_picture(image: KittyPicturePixels<'_>, width_bucket: u32) -> io::Result<EncodedPicture> {
    let [width, height] = image.image_px;
    let channels = match image.format {
        KittyImageFormat::Rgb => 3,
        KittyImageFormat::Rgba => 4,
        KittyImageFormat::Png => return Err(io::Error::other("expected decoded Kitty pixels")),
    };
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels));
    if width == 0 || height == 0 || expected_len != Some(image.data.len()) {
        return Err(io::Error::other("invalid Kitty image dimensions"));
    }
    let opaque = channels == 3 || image.data.chunks_exact(4).all(|pixel| pixel[3] == 255);
    let output_width = width_bucket.min(width);
    let output_height = ((u64::from(height) * u64::from(output_width) + u64::from(width) / 2)
        / u64::from(width))
    .max(1) as u32;
    let mut px = [output_width, output_height];
    loop {
        if px[0].max(px[1]) <= MAX_EDGE_PX {
            let data = if px == image.image_px {
                Cow::Borrowed(image.data.as_ref())
            } else {
                Cow::Owned(area_average(&image, px, channels, opaque))
            };
            let mut encoded = Vec::new();
            let media_type = if opaque {
                let color = if channels == 3 {
                    jpeg_encoder::ColorType::Rgb
                } else {
                    jpeg_encoder::ColorType::Rgba
                };
                jpeg_encoder::Encoder::new(&mut encoded, 85)
                    .encode(&data, px[0] as u16, px[1] as u16, color)
                    .map_err(io::Error::other)?;
                PanePictureMediaType::Jpeg
            } else {
                let mut encoder = png::Encoder::new(&mut encoded, px[0], px[1]);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.set_compression(png::Compression::Fast);
                encoder.write_header()?.write_image_data(&data)?;
                PanePictureMediaType::Png
            };
            if encoded.len() <= MAX_ENCODED_BYTES {
                return Ok(EncodedPicture {
                    px,
                    media_type,
                    data_b64: STANDARD.encode(encoded),
                });
            }
        }
        if px == [1, 1] {
            return Err(io::Error::other("picture could not fit in the reply limit"));
        }
        px = px.map(|edge| (edge / 2).max(1));
    }
}

/// Integrate the exact source rectangle for each destination pixel.
/// Coordinates are in units of 1/dst_width and 1/dst_height source pixels.
/// Use premultiplied RGB while averaging transparent pixels.
fn area_average(
    image: &KittyPicturePixels<'_>,
    dst: [u32; 2],
    channels: usize,
    opaque: bool,
) -> Vec<u8> {
    let [src_w, src_h] = image.image_px.map(u64::from);
    let [dst_w, dst_h] = dst.map(u64::from);
    let area = src_w * src_h;
    let mut out = Vec::with_capacity((dst_w * dst_h) as usize * channels);
    for dy in 0..dst_h {
        let (top, bottom) = (dy * src_h, (dy + 1) * src_h);
        for dx in 0..dst_w {
            let (left, right) = (dx * src_w, (dx + 1) * src_w);
            let mut rgb = [0; 3];
            let mut alpha = 0;
            for sy in top / dst_h..bottom.div_ceil(dst_h) {
                let h = bottom.min((sy + 1) * dst_h) - top.max(sy * dst_h);
                for sx in left / dst_w..right.div_ceil(dst_w) {
                    let w = right.min((sx + 1) * dst_w) - left.max(sx * dst_w);
                    let index = (sy * src_w + sx) as usize * channels;
                    let pixel = &image.data[index..index + channels];
                    let weight = w * h * if opaque { 1 } else { u64::from(pixel[3]) };
                    alpha += weight;
                    for (sum, channel) in rgb.iter_mut().zip(pixel) {
                        *sum += u64::from(*channel) * weight;
                    }
                }
            }
            let divisor = alpha.max(1);
            out.extend(rgb.map(|sum| ((sum + divisor / 2) / divisor) as u8));
            if channels == 4 {
                out.push(if opaque {
                    255
                } else {
                    ((alpha + area / 2) / area) as u8
                });
            }
        }
    }
    out
}

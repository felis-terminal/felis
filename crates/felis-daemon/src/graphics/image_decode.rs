//! Image decoding for the Kitty graphics dispatcher: payload
//! acquisition (direct / file / temp-file / shm), the per-image decode
//! budget, optional zlib inflate, and the raw and PNG decoders.

use super::{
    CompleteCommand, Cursor, ImageEntry, ImageFormat, base64, control_byte, control_u32,
    control_value, zlib,
};
// Windows declines the file-backed methods with `ENOTSUP`
// (docs/reference/protocols/support-matrix.md "Kitty graphics").
#[cfg(unix)]
use super::{Mode, OFlags};

/// Maximum bytes one decoded image may occupy.
///
/// Matches `felis_protocol::messages::MAX_IMAGE_BYTES` so decode budget and
/// client wire admission share one constant.
pub const MAX_DECODED_BYTES: usize = felis_protocol::messages::MAX_IMAGE_BYTES as usize;
// Equal to the reassembly cap so a transmission cannot eat more
// decoded memory than it did chunked.
const _: () = assert!(MAX_DECODED_BYTES == felis_vt::kitty_graphics::REASSEMBLY_BUFFER_LIMIT);

/// Reasons [`decode_image`] may reject a transmission. Mapped to Kitty
/// error codes at the action-handler layer, so the dispatcher can
/// decide whether to respond at all (`q=`) before paying the
/// `format_response` cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// `EINVAL`: control values syntactically valid but semantically
    /// wrong.
    InvalidValue(&'static str),
    /// `ENOTSUP`: something this build does not provide. The producer is
    /// expected to renegotiate, not retry, which is why these are not
    /// `EIO`.
    Unsupported(&'static str),
    /// `EBADF`: bytes present but corrupt.
    BadImage(&'static str),
    /// `ENOTSUP`-class: decoder budget exceeded. Distinct from `BadImage`
    /// so the producer knows it can retry smaller.
    OverBudget,
    /// `EIO`: filesystem failure on `t=f` / `t=t` / `t=s`. Distinct from
    /// `BadImage` so the producer can decide whether a retry is
    /// meaningful.
    IoError(&'static str),
}

/// Decode the payload of one assembled Kitty graphics command into an [`ImageEntry`].
///
/// Handles transport acquisition, base64 decoding, optional decompression,
/// and pixel format decoding by value to avoid per-frame buffer copies.
pub fn decode_image(complete: &CompleteCommand) -> Result<ImageEntry, DecodeError> {
    let raw = bytes_from_transmission(complete)?;
    let maybe_inflated = inflate_if_zlib(raw, complete)?;
    decode_format(maybe_inflated, complete)
}

fn bytes_from_transmission(complete: &CompleteCommand) -> Result<Vec<u8>, DecodeError> {
    match control_byte(complete, b't').unwrap_or(b'd') {
        b'd' => base64::decode(&complete.payload)
            .ok_or(DecodeError::BadImage("malformed base64 payload")),
        b'f' => read_file_payload(&complete.payload, MAX_DECODED_BYTES),
        b't' => read_temp_file_payload(&complete.payload, MAX_DECODED_BYTES),
        b's' => read_shm_payload(complete, MAX_DECODED_BYTES),
        _ => Err(DecodeError::InvalidValue("t= must be one of d/f/t/s")),
    }
}

/// `t=f`: read the file at the base64-encoded path.
///
/// Decoded paths must be absolute and free of `..`, opened with `NOFOLLOW`
/// on the final component, and capped at `MAX_DECODED_BYTES`
/// (`docs/explanation/security-model.md`).
#[cfg(unix)]
fn read_file_payload(payload: &[u8], cap: usize) -> Result<Vec<u8>, DecodeError> {
    let path_bytes =
        base64::decode(payload).ok_or(DecodeError::BadImage("malformed base64 path"))?;
    let path_str = std::str::from_utf8(&path_bytes)
        .map_err(|_| DecodeError::InvalidValue("t=f path is not valid UTF-8"))?;
    let path = std::path::Path::new(path_str);
    if !path.is_absolute() {
        return Err(DecodeError::InvalidValue("t=f path must be absolute"));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(DecodeError::InvalidValue(
            "t=f path must not contain '..' components",
        ));
    }
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| DecodeError::IoError("could not open t=f path"))?;
    let meta =
        rustix::fs::fstat(&fd).map_err(|_| DecodeError::IoError("could not stat t=f path"))?;
    let size = usize::try_from(meta.st_size).unwrap_or(usize::MAX);
    if size > cap {
        return Err(DecodeError::OverBudget);
    }
    // Devices / FIFOs could block the read. `From`, not `try_from`:
    // `st_mode` is `u16` on macOS and `u32` on Linux; `allow` not
    // `expect` because the conversion is useless on Linux alone.
    #[allow(clippy::useless_conversion)]
    let mode: u32 = meta.st_mode.into();
    if mode & libc_s_ifmt() != libc_s_ifreg() {
        return Err(DecodeError::InvalidValue(
            "t=f path must point at a regular file",
        ));
    }
    let mut buf = vec![0u8; size];
    let mut read = 0;
    while read < size {
        let n = rustix::io::read(&fd, &mut buf[read..])
            .map_err(|_| DecodeError::IoError("read failed on t=f file"))?;
        if n == 0 {
            // A concurrent write shrank the file; return what arrived and let
            // the format layer judge it.
            buf.truncate(read);
            break;
        }
        read += n;
    }
    Ok(buf)
}

/// `t=f` needs the `O_NOFOLLOW` open, which has no Windows equivalent
/// felis carries. `ENOTSUP`, not `EIO`: a documented
/// platform property (docs/reference/protocols/support-matrix.md
/// "Kitty graphics"); `EIO` would invite a retry loop that can never
/// succeed.
#[cfg(windows)]
const fn read_file_payload(_payload: &[u8], _cap: usize) -> Result<Vec<u8>, DecodeError> {
    Err(DecodeError::Unsupported(
        "t=f file transmission is not supported on Windows; use t=d",
    ))
}

/// `t=t`: read the temp file and unlink it.
///
/// Unlinking is enforced by felis; if unlinking fails the image is dropped.
/// Reading uses `openat(NOFOLLOW)` and `unlinkat` from the pinned parent
/// directory to resist TOCTOU symlink swaps.
#[cfg(unix)]
fn read_temp_file_payload(payload: &[u8], cap: usize) -> Result<Vec<u8>, DecodeError> {
    let path_bytes =
        base64::decode(payload).ok_or(DecodeError::BadImage("malformed base64 path"))?;
    let path_str = std::str::from_utf8(&path_bytes)
        .map_err(|_| DecodeError::InvalidValue("t=t path is not valid UTF-8"))?;
    let path = std::path::Path::new(path_str);
    if !path.is_absolute() {
        return Err(DecodeError::InvalidValue("t=t path must be absolute"));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(DecodeError::InvalidValue(
            "t=t path must not contain '..' components",
        ));
    }
    let parent = path.parent().ok_or(DecodeError::InvalidValue(
        "t=t path must have a parent directory",
    ))?;
    let name = path
        .file_name()
        .ok_or(DecodeError::InvalidValue("t=t path must end in a filename"))?;

    // `RDONLY | DIRECTORY` rather than `O_PATH`, which is Linux-only.
    let dir = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| DecodeError::IoError("could not open t=t parent directory"))?;

    let fd = rustix::fs::openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| DecodeError::IoError("could not open t=t path"))?;
    let meta =
        rustix::fs::fstat(&fd).map_err(|_| DecodeError::IoError("could not stat t=t path"))?;
    let size = usize::try_from(meta.st_size).unwrap_or(usize::MAX);
    if size > cap {
        // Still unlink: leaving an over-budget temp file behind leaks disk
        // per retry.
        let _ = rustix::fs::unlinkat(&dir, name, rustix::fs::AtFlags::empty());
        return Err(DecodeError::OverBudget);
    }
    // See `read_file_payload` for the `allow`.
    #[allow(clippy::useless_conversion)]
    let mode: u32 = meta.st_mode.into();
    if mode & libc_s_ifmt() != libc_s_ifreg() {
        let _ = rustix::fs::unlinkat(&dir, name, rustix::fs::AtFlags::empty());
        return Err(DecodeError::InvalidValue(
            "t=t path must point at a regular file",
        ));
    }

    let mut buf = vec![0u8; size];
    let mut read = 0;
    while read < size {
        let Ok(n) = rustix::io::read(&fd, &mut buf[read..]) else {
            let _ = rustix::fs::unlinkat(&dir, name, rustix::fs::AtFlags::empty());
            return Err(DecodeError::IoError("read failed on t=t file"));
        };
        if n == 0 {
            buf.truncate(read);
            break;
        }
        read += n;
    }
    drop(fd);

    rustix::fs::unlinkat(&dir, name, rustix::fs::AtFlags::empty())
        .map_err(|_| DecodeError::IoError("could not unlink t=t file after read"))?;

    Ok(buf)
}

/// Windows shim for `t=t`: see [`read_file_payload`]'s Windows shim.
#[cfg(windows)]
const fn read_temp_file_payload(_payload: &[u8], _cap: usize) -> Result<Vec<u8>, DecodeError> {
    Err(DecodeError::Unsupported(
        "t=t temp-file transmission is not supported on Windows; use t=d",
    ))
}

/// `t=s`: read the POSIX shared-memory object named by the payload.
///
/// Shared memory unlinking is deferred to session teardown
/// ([`crate::pool::Session::shm_segments`]). Uses [`normalize_shm_name`]
/// and [`read_shm_range`] to support platform differences.
#[cfg(unix)]
fn read_shm_payload(complete: &CompleteCommand, cap: usize) -> Result<Vec<u8>, DecodeError> {
    use rustix::shm;

    let name_bytes =
        base64::decode(&complete.payload).ok_or(DecodeError::BadImage("malformed base64 name"))?;
    let name = std::str::from_utf8(&name_bytes)
        .map_err(|_| DecodeError::InvalidValue("t=s name is not valid UTF-8"))?;
    let name = normalize_shm_name(name);

    let nofollow = shm::OFlags::from_bits_retain(OFlags::NOFOLLOW.bits());
    let fd = match shm::open(&*name, shm::OFlags::RDONLY | nofollow, Mode::empty()) {
        Ok(fd) => fd,
        // INVAL is rustix's name-validation rejection (embedded '/', '.',
        // '..', empty): a malformed command, so EINVAL rather than a
        // retryable EIO.
        Err(rustix::io::Errno::INVAL) => {
            return Err(DecodeError::InvalidValue(
                "t=s name is not a valid shm name",
            ));
        }
        Err(_) => return Err(DecodeError::IoError("could not open t=s shm object")),
    };

    let result = read_shm_range(&fd, complete, cap);
    drop(fd);
    result
}

/// The `t=s` object name for [`crate::pool::Session::shm_segments`];
/// `None` when the command is not `t=s` or the name is malformed
/// (never opened, so nothing to unlink).
#[must_use]
pub fn shm_segment_name(complete: &CompleteCommand) -> Option<String> {
    if control_byte(complete, b't') != Some(b's') {
        return None;
    }
    let name_bytes = base64::decode(&complete.payload)?;
    String::from_utf8(name_bytes).ok()
}

/// Unlink a deferred `t=s` object at session teardown; ENOENT (the
/// producer unlinked on exit) is expected. Normalized like
/// `read_shm_payload` opens it, so the macOS slash restore reaches the
/// producer's object.
#[cfg(unix)]
pub fn unlink_shm_segment(name: &str) {
    let _ = rustix::shm::unlink(&*normalize_shm_name(name));
}

/// Restore the leading slash a producer stripped from a `t=s` name
/// where it matters: glibc treats `foo` and `/foo` as one `/dev/shm`
/// inode, while macOS/BSD keep a literal namespace where `shm_open`
/// requires the slash. A name already carrying one is left as is.
#[cfg(all(unix, target_os = "linux"))]
const fn normalize_shm_name(name: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(name)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn normalize_shm_name(name: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if name.starts_with('/') {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("/{name}"))
    }
}

/// Windows has no POSIX shared memory; `t=s` is declined at decode
/// time.
#[cfg(windows)]
pub const fn unlink_shm_segment(_name: &str) {}

/// Bounded, LRU-ordered queue of deferred `t=s` object names.
///
/// Bounded to prevent memory growth from rotating producer-chosen names.
/// Evicts the oldest name when full while refreshing reused names.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShmDeferral {
    /// Oldest first; a linear scan at `CAP` entries beats a set's hashing
    /// and keeps the recency order.
    names: std::collections::VecDeque<String>,
}

impl ShmDeferral {
    /// Sized for mpv's single reused name and a compositing producer's
    /// handful.
    pub const CAP: usize = 16;

    /// Defer `name`'s unlink, returning the name evicted to make room,
    /// which the caller must unlink. A name already deferred is refreshed,
    /// so a producer streaming into one segment keeps its slot.
    #[must_use]
    pub fn record(&mut self, name: String) -> Option<String> {
        if let Some(at) = self.names.iter().position(|held| *held == name) {
            self.names.remove(at);
            self.names.push_back(name);
            return None;
        }
        self.names.push_back(name);
        if self.names.len() > Self::CAP {
            return self.names.pop_front();
        }
        None
    }

    /// The deferred names, oldest first.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }
}

/// The fstat + budget + `S=`/`O=` range logic, then the platform
/// byte-copy.
#[cfg(unix)]
fn read_shm_range(
    fd: &rustix::fd::OwnedFd,
    complete: &CompleteCommand,
    cap: usize,
) -> Result<Vec<u8>, DecodeError> {
    let meta = rustix::fs::fstat(fd).map_err(|_| DecodeError::IoError("could not stat t=s"))?;
    // A same-UID process could plant a FIFO at the name and stall the
    // read loop. Linux-only: macOS keeps POSIX shm in a kernel namespace
    // where `shm_open` can only return a real object, and `fstat` on it
    // reports no file-type bits (`S_IFMT == 0`), so requiring `S_IFREG`
    // would reject every legitimate segment.
    #[cfg(target_os = "linux")]
    {
        #[allow(clippy::useless_conversion)]
        let mode: u32 = meta.st_mode.into();
        if mode & libc_s_ifmt() != libc_s_ifreg() {
            return Err(DecodeError::InvalidValue(
                "t=s name must be a shared-memory object",
            ));
        }
    }
    let size = usize::try_from(meta.st_size).unwrap_or(usize::MAX);
    let offset = control_u32(complete, b'O').unwrap_or(0) as usize;
    if offset > size {
        return Err(DecodeError::InvalidValue("O= is past the end of the data"));
    }
    let len = match control_u32(complete, b'S') {
        Some(s) => {
            let s = s as usize;
            if s > size - offset {
                return Err(DecodeError::InvalidValue("S= is past the end of the data"));
            }
            s
        }
        None => size - offset,
    };
    if len > cap {
        return Err(DecodeError::OverBudget);
    }
    #[cfg(target_os = "linux")]
    {
        copy_shm_range_pread(fd, offset, len)
    }
    #[cfg(not(target_os = "linux"))]
    {
        copy_shm_range_mmap(fd, offset, len)
    }
}

/// `pread` into uninitialized spare capacity
/// (`rustix::buffer::spare_capacity`), not `vec![0; len]`: a fullscreen
/// mpv frame is >10 MiB at 24-30 fps, and the zero fill would touch
/// every page twice.
#[cfg(target_os = "linux")]
fn copy_shm_range_pread(
    fd: &rustix::fd::OwnedFd,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, DecodeError> {
    let mut buf = Vec::with_capacity(len);
    while buf.len() < len {
        let pos = (offset + buf.len()) as u64;
        let n = rustix::io::pread(fd, rustix::buffer::spare_capacity(&mut buf), pos)
            .map_err(|_| DecodeError::IoError("read failed on t=s shm object"))?;
        if n == 0 {
            // A concurrent ftruncate shrank the object; the format layer judges
            // what arrived.
            break;
        }
    }
    // `with_capacity` may round the allocation up, letting the final
    // pread run past `len`; trim so `S=` subranges stay exact.
    buf.truncate(len);
    Ok(buf)
}

/// macOS byte-copy via `mmap`, compiled on Unix for testing.
///
/// Maps from the page boundary at or below `offset` and copies `len` bytes
/// starting at `delta` within the mapped window.
#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[allow(unsafe_code)]
fn copy_shm_range_mmap(
    fd: &rustix::fd::OwnedFd,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, DecodeError> {
    use rustix::mm::{MapFlags, ProtFlags};

    // `mmap` rejects a zero length with EINVAL.
    if len == 0 {
        return Ok(Vec::new());
    }
    let page = rustix::param::page_size();
    let map_off = offset & !(page - 1);
    let delta = offset - map_off;
    let map_len = delta + len;

    // SAFETY: `fd` is a live, read-only shm descriptor (opened just above
    // in `read_shm_payload`). `map_off` is page-aligned (masked to the
    // page boundary) as `mmap` requires, and `map_len > 0`. We request a
    // private read-only view (`PROT_READ`, `MAP_SHARED`); on success the
    // kernel returns a mapping of exactly `map_len` accessible bytes.
    let ptr = unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            map_len,
            ProtFlags::READ,
            MapFlags::SHARED,
            fd,
            map_off as u64,
        )
    }
    .map_err(|_| DecodeError::IoError("could not mmap t=s shm object"))?;

    // SAFETY: `mmap` returned a valid mapping spanning `[ptr, ptr +
    // map_len)`. `delta + len == map_len`, so `[ptr + delta, ptr + delta
    // + len)` is fully inside it; `ptr` is page-aligned (well above the
    // `u8` alignment of 1). The bytes are file-backed for `[offset,
    // offset + len)` per the caller's `fstat` check, so reading them is
    // defined barring a concurrent shrink (documented above). The slice
    // is consumed (copied) before `munmap` below, so it never outlives
    // the mapping.
    let view = unsafe { core::slice::from_raw_parts(ptr.cast::<u8>().add(delta), len) };
    let out = view.to_vec();

    // SAFETY: `ptr`/`map_len` are exactly the pair `mmap` returned and we
    // have not unmapped them yet; `view` is not used after the copy
    // above, so unmapping here invalidates no live reference.
    unsafe {
        let _ = rustix::mm::munmap(ptr, map_len);
    }
    Ok(out)
}

/// Windows shim for `t=s`: see [`read_file_payload`]'s Windows shim.
#[cfg(windows)]
const fn read_shm_payload(
    _complete: &CompleteCommand,
    _cap: usize,
) -> Result<Vec<u8>, DecodeError> {
    Err(DecodeError::Unsupported(
        "t=s shared-memory transmission is not supported on Windows; use t=d",
    ))
}

/// Stand-in for `libc::S_IFMT`, so `libc` is not pulled in to mask one
/// field; the bits are stable across every Unix felis runs on.
#[cfg(unix)]
const fn libc_s_ifmt() -> u32 {
    0o170_000
}

/// Local stand-in for `libc::S_IFREG`.
#[cfg(unix)]
const fn libc_s_ifreg() -> u32 {
    0o100_000
}

/// `o=z` inflates under the per-image cap; absent passes through.
fn inflate_if_zlib(bytes: Vec<u8>, complete: &CompleteCommand) -> Result<Vec<u8>, DecodeError> {
    let Some(o) = control_value(complete, b'o') else {
        return Ok(bytes);
    };
    if o != b"z" {
        return Err(DecodeError::Unsupported("o= must be z (zlib) or absent"));
    }
    match zlib::inflate(&bytes, MAX_DECODED_BYTES) {
        Ok(out) => Ok(out),
        Err(zlib::InflateError::SizeLimit) => Err(DecodeError::OverBudget),
        Err(zlib::InflateError::Decompression) => Err(DecodeError::BadImage(
            "zlib stream malformed or checksum failed",
        )),
        // `InflateError` is `#[non_exhaustive]`; EBADF is the closest Kitty
        // code for an unmapped variant.
        Err(_) => Err(DecodeError::BadImage("zlib inflate failed")),
    }
}

/// `f=` defaults to `32` (RGBA), Kitty's documented default.
fn decode_format(bytes: Vec<u8>, complete: &CompleteCommand) -> Result<ImageEntry, DecodeError> {
    let f = control_value(complete, b'f').unwrap_or(b"32");
    match f {
        b"24" => decode_raw(bytes, complete, ImageFormat::Rgb24, 3),
        b"32" => decode_raw(bytes, complete, ImageFormat::Rgba32, 4),
        b"100" => decode_png(&bytes),
        other => {
            let _ = other;
            Err(DecodeError::InvalidValue(
                "f= must be 24 (RGB), 32 (RGBA), or 100 (PNG)",
            ))
        }
    }
}

/// `f=24` / `f=32`: raw pixels, `s=` × `v=`.
///
/// Payloads must hold at least `s × v × bpp` bytes: shm segments may round up
/// to page boundaries, so reading back can exceed the declared frame size.
fn decode_raw(
    mut bytes: Vec<u8>,
    complete: &CompleteCommand,
    format: ImageFormat,
    bpp: usize,
) -> Result<ImageEntry, DecodeError> {
    let width = control_u32(complete, b's')
        .ok_or(DecodeError::InvalidValue("raw transmission requires s="))?;
    let height = control_u32(complete, b'v')
        .ok_or(DecodeError::InvalidValue("raw transmission requires v="))?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(bpp))
        .ok_or(DecodeError::InvalidValue("s × v × bpp overflows usize"))?;
    if expected > MAX_DECODED_BYTES {
        return Err(DecodeError::OverBudget);
    }
    if bytes.len() < expected {
        return Err(DecodeError::InvalidValue(
            "raw payload shorter than s × v × bpp",
        ));
    }
    // `ImageStore::insert` trusts the dispatcher for the buffer matching
    // its declared shape.
    bytes.truncate(expected);
    Ok(ImageEntry::new(width, height, format, bytes))
}

/// `f=100`: PNG under `Limits { bytes: MAX_DECODED_BYTES }`, normalized
/// to RGBA8 because the client surface renders only RGBA.
fn decode_png(bytes: &[u8]) -> Result<ImageEntry, DecodeError> {
    let bad = |_| DecodeError::BadImage("PNG decode failed");
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_DECODED_BYTES,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(bad)?;
    // `None`: the dimensions overflow byte arithmetic.
    let size = reader
        .output_buffer_size()
        .filter(|&s| s <= MAX_DECODED_BYTES)
        .ok_or(DecodeError::OverBudget)?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(bad)?;
    buf.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => widen::<3>(&buf, |[r, g, b]| [r, g, b, 0xFF]),
        png::ColorType::Grayscale => widen::<1>(&buf, |[g]| [g, g, g, 0xFF]),
        png::ColorType::GrayscaleAlpha => widen::<2>(&buf, |[g, a]| [g, g, g, a]),
        // `normalize_to_color8` expands palettes before the frame is handed
        // back.
        png::ColorType::Indexed => return Err(DecodeError::BadImage("PNG decode failed")),
    };
    if pixels.len() > MAX_DECODED_BYTES {
        // The decoder limit caps its own buffer; RGBA widening can still
        // grow past it (RGB decodes at ¾ the size).
        return Err(DecodeError::OverBudget);
    }
    Ok(ImageEntry::new(
        info.width,
        info.height,
        ImageFormat::Rgba32,
        pixels,
    ))
}

/// A remainder shorter than `N` cannot occur (the decoder sizes
/// frames exactly).
fn widen<const N: usize>(buf: &[u8], px: impl Fn([u8; N]) -> [u8; 4]) -> Vec<u8> {
    let (chunks, _) = buf.as_chunks::<N>();
    let mut out = vec![0; chunks.len() * 4];
    for (d, &s) in out.as_chunks_mut::<4>().0.iter_mut().zip(chunks) {
        *d = px(s);
    }
    out
}

/// Linux-hosted verification of the macOS `t=s` mmap path: Linux shm
/// objects map identically, so CI proves it without a Mac.
#[cfg(all(test, target_os = "linux"))]
mod mmap_tests {
    // `cfg(all(test, …))` isn't the bare `cfg(test)` clippy's
    // `allow-expect-in-tests` keys on, so opt in explicitly.
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    /// The caller unlinks.
    fn make_shm(suffix: &str, bytes: &[u8]) -> String {
        use std::io::Write;
        let name = format!("/felis-mmap-test-{}-{suffix}", std::process::id());
        let fd = rustix::shm::open(
            &name,
            rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
            Mode::RUSR | Mode::WUSR,
        )
        .expect("create test shm");
        std::fs::File::from(fd)
            .write_all(bytes)
            .expect("fill test shm");
        name
    }

    fn open_ro(name: &str) -> rustix::fd::OwnedFd {
        rustix::shm::open(name, rustix::shm::OFlags::RDONLY, Mode::empty())
            .expect("reopen test shm")
    }

    #[test]
    fn mmap_copies_whole_object() {
        let bytes: Vec<u8> = (0u8..200).collect();
        let name = make_shm("whole", &bytes);
        let fd = open_ro(&name);
        let got = copy_shm_range_mmap(&fd, 0, bytes.len()).expect("mmap read");
        assert_eq!(got, bytes);
        let _ = rustix::shm::unlink(&name);
    }

    #[test]
    fn mmap_copies_page_unaligned_subrange() {
        let bytes: Vec<u8> = (0u8..=255).cycle().take(9000).collect();
        let name = make_shm("subrange", &bytes);
        let fd = open_ro(&name);
        let (offset, len) = (4097usize, 1000usize); // crosses a 4 KiB page
        let got = copy_shm_range_mmap(&fd, offset, len).expect("mmap subrange");
        assert_eq!(got, &bytes[offset..offset + len]);
        let _ = rustix::shm::unlink(&name);
    }

    #[test]
    fn mmap_zero_length_is_empty_without_mapping() {
        let name = make_shm("empty", &[1, 2, 3, 4]);
        let fd = open_ro(&name);
        assert_eq!(
            copy_shm_range_mmap(&fd, 0, 0).expect("zero len ok"),
            Vec::<u8>::new()
        );
        let _ = rustix::shm::unlink(&name);
    }

    #[test]
    fn mmap_and_pread_agree() {
        let bytes: Vec<u8> = (0u8..=255).cycle().take(20_000).collect();
        let name = make_shm("agree", &bytes);
        let fd = open_ro(&name);
        for &(o, l) in &[(0usize, 20_000usize), (100, 5000), (8191, 4096)] {
            let via_mmap = copy_shm_range_mmap(&fd, o, l).expect("mmap");
            let via_pread = copy_shm_range_pread(&fd, o, l).expect("pread");
            assert_eq!(via_mmap, via_pread, "offset={o} len={l}");
        }
        let _ = rustix::shm::unlink(&name);
    }
}

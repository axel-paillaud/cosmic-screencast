//! Reads a DMA-BUF (a buffer living in GPU memory) from the CPU.
//!
//! With DMA-BUF, the compositor copies the screen GPU → GPU, which is cheap and
//! doesn't stall it. We then read only the rows of the selected region. This
//! works because we ask for the LINEAR layout (plain rows of pixels): other
//! layouts are tiled in GPU-specific ways.
//!
//! GPU memory mapped on the CPU is usually not cached: reading it is slow, so
//! callers should copy what they need in big chunks (whole rows), not pixel by
//! pixel.

use std::io;
use std::os::fd::RawFd;
use std::ptr::NonNull;

/// DRM format modifier meaning "plain rows of pixels" (`DRM_FORMAT_MOD_LINEAR`).
pub const MODIFIER_LINEAR: i64 = 0;

// From <linux/dma-buf.h>.
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
/// `_IOW('b', 0, struct dma_buf_sync)`
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;

/// A DMA-BUF mapped in memory, ready to be read. Unmapped when dropped.
pub struct Mapping {
    fd: RawFd,
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    /// Maps the first `len` bytes of the DMA-BUF `fd` for reading.
    ///
    /// The fd is only borrowed: it must stay open while the mapping lives
    /// (it belongs to the PipeWire buffer, which we hold during the read).
    pub fn new(fd: RawFd, len: usize) -> io::Result<Mapping> {
        // SAFETY: plain system call; the result is checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mapping = Mapping {
            fd,
            ptr: NonNull::new(ptr.cast()).expect("mmap never returns null on success"),
            len,
        };
        // Wait until the GPU has finished writing, and make the data visible
        // to the CPU. On error, `mapping` is dropped, which unmaps it.
        sync(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)?;
        Ok(mapping)
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` readable bytes until `drop`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = sync(self.fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        // SAFETY: unmaps exactly what `new` mapped.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// `DMA_BUF_IOCTL_SYNC`: brackets CPU access to the buffer.
fn sync(fd: RawFd, flags: u64) -> io::Result<()> {
    loop {
        // SAFETY: the ioctl reads one u64 (`struct dma_buf_sync`).
        let ret = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &flags) };
        if ret == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        // Interrupted by a signal (e.g. Ctrl+C) or "busy": just try again.
        if !matches!(
            err.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
        ) {
            return Err(err);
        }
    }
}

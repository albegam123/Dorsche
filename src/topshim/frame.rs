use std::{
    fs::File,
    io::{self, Read},
    mem::{align_of, size_of},
    ops::Deref,
    os::fd::FromRawFd,
    sync::{
        Arc,
        atomic::{Ordering, fence},
    },
};

use anyhow::{Context, Result, anyhow, ensure};
use memmap2::{Mmap, MmapOptions};
use tokio::io::unix::AsyncFd;

use crate::bridge::ffi;

/// The unit of ownership on every Rust-side audio edge.
pub type AudioFrame = Arc<SharedPcmFrame>;

/// An immutable, ref-counted view of C++-produced DMA/shared pages.
///
/// A literal `Arc<[f32]>` cannot adopt an existing `mmap`: stable `Arc` requires
/// its own allocator layout with a refcount header and therefore must copy.
/// `Arc<SharedPcmFrame>` is the zero-copy equivalent: it owns the mapping lease
/// and dereferences to `[f32]`, so consumers retain exactly the same ergonomics
/// without lying about allocation provenance.
pub struct SharedPcmFrame {
    // Keep the fd for explicit lease semantics. Linux mappings outlive close(),
    // but retaining it also permits future DMA_BUF_IOCTL_SYNC integration.
    _buffer: File,
    mapping: Mmap,
    sample_count: usize,
    sample_rate: u32,
    sequence: u64,
    capture_time_ns: u64,
    dma_buf: bool,
}

impl SharedPcmFrame {
    /// Consume the unique C++ object and turn its two descriptors into Rust
    /// owned resources. There is no duplicated descriptor and no PCM memcpy.
    pub async fn from_cpp(mut raw: cxx::UniquePtr<ffi::HardwareFrame>) -> Result<AudioFrame> {
        ensure!(!raw.is_null(), "C++ returned a null capture frame");

        let sample_count = raw.sample_count();
        let sample_rate = raw.sample_rate();
        let sequence = raw.sequence();
        let capture_time_ns = raw.capture_time_ns();
        let dma_buf = raw.is_dma_buf();
        let byte_count = sample_count
            .checked_mul(size_of::<f32>())
            .ok_or_else(|| anyhow!("PCM byte length overflow"))?;

        // `take_*` is the linearization point: C++ exchanges each fd with -1.
        // FromRawFd is sound precisely because no C++ destructor can close it.
        let buffer_fd = raw.pin_mut().take_buffer_fd();
        let fence_fd = raw.pin_mut().take_fence_fd();
        ensure!(buffer_fd >= 0, "C++ transferred an invalid buffer fd");
        if fence_fd < 0 {
            // Reconstitute ownership so the already-transferred buffer closes.
            drop(unsafe { File::from_raw_fd(buffer_fd) });
            return Err(anyhow!("C++ transferred an invalid fence fd"));
        }
        let buffer = unsafe { File::from_raw_fd(buffer_fd) };
        let fence_file = unsafe { File::from_raw_fd(fence_fd) };
        drop(raw); // metadata copied; C++ now owns neither descriptor.

        wait_for_fence(fence_file).await.context("capture fence")?;

        // SAFETY: the fd owns at least byte_count bytes, C++ has ended writable
        // CPU access, and the completed fence gives us an immutable read lease.
        let mapping = unsafe { MmapOptions::new().len(byte_count).map(&buffer) }
            .context("read-only map of capture buffer")?;
        ensure!(mapping.len() == byte_count, "short PCM mapping");
        ensure!(
            (mapping.as_ptr() as usize).is_multiple_of(align_of::<f32>()),
            "PCM mapping is not f32-aligned"
        );

        Ok(Arc::new(Self {
            _buffer: buffer,
            mapping,
            sample_count,
            sample_rate,
            sequence,
            capture_time_ns,
            dma_buf,
        }))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn capture_time_ns(&self) -> u64 {
        self.capture_time_ns
    }

    pub fn is_dma_buf(&self) -> bool {
        self.dma_buf
    }
}

impl Deref for SharedPcmFrame {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        // SAFETY: construction validated length/alignment. Mmap is read-only,
        // lives as long as this value, and sample_count exactly spans it.
        unsafe {
            std::slice::from_raw_parts(self.mapping.as_ptr().cast::<f32>(), self.sample_count)
        }
    }
}

async fn wait_for_fence(fence_file: File) -> io::Result<()> {
    let fence_fd = AsyncFd::new(fence_file)?;
    loop {
        let mut ready = fence_fd.readable().await?;
        let mut signal = [0_u8; size_of::<u64>()];
        let mut file_ref = ready.get_inner();
        match file_ref.read_exact(&mut signal) {
            Ok(()) => {
                // Pairs with the producer's release fence. For a real sync_file,
                // poll-readiness is supplied by the kernel DMA fence instead.
                fence(Ordering::Acquire);
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                ready.clear_ready();
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cpp_frame_becomes_a_shared_read_only_lease() {
        let mut controller = ffi::new_hardware_controller();
        let raw = controller.pin_mut().capture_next().unwrap();
        let frame = SharedPcmFrame::from_cpp(raw).await.unwrap();
        let second_consumer = Arc::clone(&frame);

        assert_eq!(frame.sample_rate(), 16_000);
        assert_eq!(frame.len(), 160);
        assert_eq!(frame.sequence(), 0);
        assert!(Arc::ptr_eq(&frame, &second_consumer));
        assert!(frame.iter().all(|sample| sample.is_finite()));
    }
}

//! The Floss-style topshim ABI.  No `void*`, callback trampoline, or borrowed
//! hardware pointer is allowed to cross this file.

#[cxx::bridge(namespace = "dorsche::ffi")]
pub mod ffi {
    /// A scheduling receipt, not audio storage. The samples stay in their
    /// original allocation while C++ synchronously submits the borrowed slice.
    struct PlaybackReceipt {
        sequence: u64,
        presentation_time_ns: u64,
    }

    unsafe extern "C++" {
        include!("dorsche_core.h");

        type HardwareController;
        type HardwareFrame;

        fn new_hardware_controller() -> UniquePtr<HardwareController>;

        /// Real implementations issue a non-blocking dequeue ioctl here. The
        /// demo allocates/fills a DMA-heap buffer (or memfd fallback).
        fn capture_next(self: Pin<&mut HardwareController>) -> Result<UniquePtr<HardwareFrame>>;

        /// `&[f32]` becomes `rust::Slice<const float>`: it is borrowed only for
        /// this call and cxx prevents C++ from retaining it after return.
        fn play_pcm(
            self: Pin<&mut HardwareController>,
            samples: &[f32],
            sequence: u64,
        ) -> PlaybackReceipt;

        fn sample_count(self: &HardwareFrame) -> usize;
        fn sample_rate(self: &HardwareFrame) -> u32;
        fn sequence(self: &HardwareFrame) -> u64;
        fn capture_time_ns(self: &HardwareFrame) -> u64;
        fn is_dma_buf(self: &HardwareFrame) -> bool;

        /// These two calls are linear ownership moves. After either succeeds,
        /// the C++ destructor no longer closes that descriptor.
        fn take_buffer_fd(self: Pin<&mut HardwareFrame>) -> i32;
        fn take_fence_fd(self: Pin<&mut HardwareFrame>) -> i32;
    }
}

// SAFETY: both C++ classes are uniquely owned (`UniquePtr`), have no callbacks
// or thread affinity, and are never concurrently accessed. HardwareController
// lives exclusively inside the proxy actor; HardwareFrame is consumed exactly
// once by the fd-transfer routine. Deliberately do not implement `Sync`.
unsafe impl Send for ffi::HardwareController {}
unsafe impl Send for ffi::HardwareFrame {}

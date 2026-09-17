use std::{
    io,
    marker::PhantomData,
    mem::{align_of, size_of},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    ptr::{self, NonNull},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow, ensure};
use tokio::io::unix::AsyncFd;

use super::types::{FrameDescriptor, WIRELESS_AUDIO_ABI_VERSION};

const RING_MAGIC: u32 = u32::from_le_bytes(*b"DRNG");
const CACHE_LINE: usize = 64;

#[repr(C, align(64))]
struct RingHeader {
    magic: u32,
    abi_version: u16,
    header_bytes: u16,
    slot_count: u32,
    slot_stride: u32,
    payload_capacity: u32,
    _reserved0: [u8; 44],
    // Producer and consumer cursors occupy separate cache lines. Only one
    // process writes each cursor; acquire/release publishes slot ownership.
    head: AtomicU64,
    _producer_pad: [u8; 56],
    tail: AtomicU64,
    _consumer_pad: [u8; 56],
    dropped: AtomicU64,
    _stats_pad: [u8; 56],
}

const _: () = assert!(size_of::<RingHeader>().is_multiple_of(CACHE_LINE));

struct Mapping {
    address: NonNull<u8>,
    len: usize,
}

impl Mapping {
    fn shared(fd: RawFd, len: usize) -> io::Result<Self> {
        // SAFETY: mmap returns a new process mapping. Its lifetime is owned by
        // this object and Drop invokes munmap exactly once.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: NonNull::new(address.cast()).expect("mmap returned null"),
            len,
        })
    }

    fn header(&self) -> &RingHeader {
        // SAFETY: RingPair construction places an aligned RingHeader at byte 0.
        unsafe { &*self.address.as_ptr().cast::<RingHeader>() }
    }

    fn slot(&self, index: u64) -> *mut u8 {
        let header = self.header();
        let offset = size_of::<RingHeader>()
            + index as usize % header.slot_count as usize * header.slot_stride as usize;
        // SAFETY: index is reduced modulo slot_count and layout size was
        // checked when the mapping was constructed.
        unsafe { self.address.as_ptr().add(offset) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: address/len are the unchanged values returned by mmap.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.len) };
    }
}

// The mapping is moved into exactly one SPSC endpoint and neither endpoint is
// Sync. Cross-thread transfer is safe; concurrent access is governed solely by
// the shared atomic cursors, never by aliased Rust references.
unsafe impl Send for Mapping {}

pub struct RingPair;

/// The two descriptors transferred with SCM_RIGHTS when a media edge crosses
/// a process boundary. Conversion into or out of this object is linear: it
/// consumes the local endpoint, preserving the single-producer/single-consumer
/// invariant instead of creating a second writer or reader.
pub struct RingDescriptor {
    memory_fd: OwnedFd,
    event_fd: OwnedFd,
}

impl RingDescriptor {
    pub fn from_owned_fds(memory_fd: OwnedFd, event_fd: OwnedFd) -> Self {
        Self {
            memory_fd,
            event_fd,
        }
    }

    pub fn memory_fd(&self) -> BorrowedFd<'_> {
        self.memory_fd.as_fd()
    }

    pub fn event_fd(&self) -> BorrowedFd<'_> {
        self.event_fd.as_fd()
    }

    pub fn into_owned_fds(self) -> (OwnedFd, OwnedFd) {
        (self.memory_fd, self.event_fd)
    }
}

impl RingPair {
    /// Create one producer and one consumer over a sealed-shape memfd.
    /// Payload bytes are written directly into a reserved slot, so the normal
    /// path does not allocate and does not copy between the two processes.
    pub fn create(slot_count: u32, payload_capacity: u32) -> Result<(Producer, Consumer)> {
        ensure!(slot_count >= 2, "ring requires at least two slots");
        ensure!(
            payload_capacity > 0,
            "ring payload capacity must be non-zero"
        );

        let descriptor_bytes = size_of::<FrameDescriptor>();
        let slot_stride = align_up(
            descriptor_bytes
                .checked_add(payload_capacity as usize)
                .ok_or_else(|| anyhow!("slot size overflow"))?,
            CACHE_LINE,
        );
        let map_len = size_of::<RingHeader>()
            .checked_add(
                slot_stride
                    .checked_mul(slot_count as usize)
                    .ok_or_else(|| anyhow!("ring size overflow"))?,
            )
            .ok_or_else(|| anyhow!("ring size overflow"))?;
        ensure!(slot_stride <= u32::MAX as usize, "slot stride exceeds ABI");

        let memory_fd = create_memfd("dorsche-media-ring", map_len)?;
        let consumer_memory_fd = duplicate_fd(memory_fd.as_raw_fd())?;
        let event_fd = create_eventfd()?;
        let consumer_event_fd = duplicate_fd(event_fd.as_raw_fd())?;

        let producer_map = Mapping::shared(memory_fd.as_raw_fd(), map_len)?;
        ensure!(
            (producer_map.address.as_ptr() as usize).is_multiple_of(align_of::<RingHeader>()),
            "mmap did not satisfy atomic alignment"
        );
        // SAFETY: this is the only mapping visible before initialization and
        // the backing memfd was just created and sized by this function.
        unsafe {
            ptr::write(
                producer_map.address.as_ptr().cast::<RingHeader>(),
                RingHeader {
                    magic: RING_MAGIC,
                    abi_version: WIRELESS_AUDIO_ABI_VERSION,
                    header_bytes: size_of::<RingHeader>() as u16,
                    slot_count,
                    slot_stride: slot_stride as u32,
                    payload_capacity,
                    _reserved0: [0; 44],
                    head: AtomicU64::new(0),
                    _producer_pad: [0; 56],
                    tail: AtomicU64::new(0),
                    _consumer_pad: [0; 56],
                    dropped: AtomicU64::new(0),
                    _stats_pad: [0; 56],
                },
            );
        }

        let consumer_map = Mapping::shared(consumer_memory_fd.as_raw_fd(), map_len)?;
        validate_header(consumer_map.header(), map_len)?;

        Ok((
            Producer {
                _memory_fd: memory_fd,
                event_fd,
                mapping: producer_map,
            },
            Consumer {
                _memory_fd: consumer_memory_fd,
                event_fd: AsyncFd::new(consumer_event_fd)?,
                mapping: consumer_map,
            },
        ))
    }
}

pub struct Producer {
    _memory_fd: OwnedFd,
    event_fd: OwnedFd,
    mapping: Mapping,
}

impl Producer {
    pub fn from_descriptor(descriptor: RingDescriptor) -> Result<Self> {
        let (memory_fd, event_fd) = descriptor.into_owned_fds();
        let map_len = fd_len(memory_fd.as_raw_fd())?;
        let mapping = Mapping::shared(memory_fd.as_raw_fd(), map_len)?;
        validate_header(mapping.header(), map_len)?;
        Ok(Self {
            _memory_fd: memory_fd,
            event_fd,
            mapping,
        })
    }

    pub fn into_descriptor(self) -> RingDescriptor {
        let Self {
            _memory_fd,
            event_fd,
            mapping,
        } = self;
        drop(mapping);
        RingDescriptor::from_owned_fds(_memory_fd, event_fd)
    }

    /// Reserve a writable shared slot. The caller should read/decode directly
    /// into `payload_mut`; commit is the sole publication point.
    pub fn try_reserve(&mut self) -> Option<ProducerLease<'_>> {
        let header = self.mapping.header();
        let head = header.head.load(Ordering::Relaxed);
        let tail = header.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= u64::from(header.slot_count) {
            header.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let slot = self.mapping.slot(head);
        Some(ProducerLease {
            producer: self,
            slot,
            head,
            committed: false,
        })
    }

    /// Convenience for non-realtime callers. This performs one explicit copy;
    /// realtime backends should use `try_reserve` and fill the slot directly.
    pub fn try_push_copy(&mut self, descriptor: FrameDescriptor, payload: &[u8]) -> Result<bool> {
        let Some(mut lease) = self.try_reserve() else {
            return Ok(false);
        };
        ensure!(
            payload.len() <= lease.capacity(),
            "payload exceeds ring slot"
        );
        lease.payload_mut()[..payload.len()].copy_from_slice(payload);
        lease.commit(descriptor, payload.len())?;
        Ok(true)
    }

    pub fn dropped_frames(&self) -> u64 {
        self.mapping.header().dropped.load(Ordering::Relaxed)
    }
}

pub struct ProducerLease<'a> {
    producer: &'a mut Producer,
    slot: *mut u8,
    head: u64,
    committed: bool,
}

impl ProducerLease<'_> {
    pub fn capacity(&self) -> usize {
        self.producer.mapping.header().payload_capacity as usize
    }

    pub fn payload_mut(&mut self) -> &mut [u8] {
        // SAFETY: an SPSC producer exclusively owns this slot until commit and
        // the returned slice is bounded by payload_capacity.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.slot.add(size_of::<FrameDescriptor>()),
                self.capacity(),
            )
        }
    }

    pub fn commit(mut self, mut descriptor: FrameDescriptor, payload_bytes: usize) -> Result<()> {
        ensure!(!self.committed, "ring reservation already committed");
        ensure!(
            payload_bytes <= self.capacity(),
            "payload exceeds ring slot"
        );
        ensure!(
            payload_bytes <= u32::MAX as usize,
            "payload length exceeds ABI"
        );
        descriptor.abi_version = WIRELESS_AUDIO_ABI_VERSION;
        descriptor.header_bytes = size_of::<FrameDescriptor>() as u16;
        descriptor.payload_bytes = payload_bytes as u32;

        // SAFETY: slot is descriptor-aligned because both header size and slot
        // stride are cache-line aligned; producer has exclusive ownership.
        unsafe { ptr::write(self.slot.cast::<FrameDescriptor>(), descriptor) };
        // This release makes descriptor and payload stores visible before the
        // consumer observes head. The eventfd is only a wakeup, not the fence.
        self.producer
            .mapping
            .header()
            .head
            .store(self.head.wrapping_add(1), Ordering::Release);
        signal_eventfd(self.producer.event_fd.as_raw_fd())?;
        self.committed = true;
        Ok(())
    }
}

pub struct Consumer {
    _memory_fd: OwnedFd,
    event_fd: AsyncFd<OwnedFd>,
    mapping: Mapping,
}

impl Consumer {
    pub fn from_descriptor(descriptor: RingDescriptor) -> Result<Self> {
        let (memory_fd, event_fd) = descriptor.into_owned_fds();
        let map_len = fd_len(memory_fd.as_raw_fd())?;
        let mapping = Mapping::shared(memory_fd.as_raw_fd(), map_len)?;
        validate_header(mapping.header(), map_len)?;
        Ok(Self {
            _memory_fd: memory_fd,
            event_fd: AsyncFd::new(event_fd)?,
            mapping,
        })
    }

    pub fn into_descriptor(self) -> RingDescriptor {
        let Self {
            _memory_fd,
            event_fd,
            mapping,
        } = self;
        drop(mapping);
        RingDescriptor::from_owned_fds(_memory_fd, event_fd.into_inner())
    }

    /// Acquire one immutable zero-copy frame lease. Dropping it returns the
    /// slot; retaining it deliberately applies backpressure to the producer.
    pub fn try_acquire(&mut self) -> Result<Option<FrameLease<'_>>> {
        let header = self.mapping.header();
        let tail = header.tail.load(Ordering::Relaxed);
        let head = header.head.load(Ordering::Acquire);
        if tail == head {
            return Ok(None);
        }
        let slot = self.mapping.slot(tail);
        // SAFETY: producer published head with Release after fully writing the
        // descriptor. Acquire above prevents reading a partially written slot.
        let descriptor = unsafe { ptr::read(slot.cast::<FrameDescriptor>()) };
        ensure!(
            descriptor.abi_version == WIRELESS_AUDIO_ABI_VERSION,
            "frame ABI mismatch"
        );
        ensure!(
            descriptor.header_bytes as usize == size_of::<FrameDescriptor>(),
            "frame descriptor size mismatch"
        );
        ensure!(
            descriptor.payload_bytes <= header.payload_capacity,
            "corrupt frame payload length"
        );
        let payload = unsafe { slot.add(size_of::<FrameDescriptor>()) };
        Ok(Some(FrameLease {
            consumer: self,
            descriptor,
            payload,
            tail,
            _not_send_sync: PhantomData,
        }))
    }

    /// Wait for an eventfd notification. Always retry `try_acquire` after this
    /// call: eventfd counts wakeups, while the atomic cursors are authoritative.
    pub async fn wait_readable(&self) -> io::Result<()> {
        loop {
            let mut guard = self.event_fd.readable().await?;
            let mut value = 0_u64;
            // SAFETY: eventfd reads exactly one native u64.
            let result = unsafe {
                libc::read(
                    self.event_fd.get_ref().as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    size_of::<u64>(),
                )
            };
            if result == size_of::<u64>() as isize {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(error);
        }
    }

    pub fn queued_frames(&self) -> u64 {
        let header = self.mapping.header();
        header
            .head
            .load(Ordering::Acquire)
            .wrapping_sub(header.tail.load(Ordering::Relaxed))
    }
}

pub struct FrameLease<'a> {
    consumer: &'a mut Consumer,
    descriptor: FrameDescriptor,
    payload: *const u8,
    tail: u64,
    // A borrowed ring slot must be consumed on the acquiring task. This also
    // prevents an application from constructing concurrent consumer access.
    _not_send_sync: PhantomData<*mut ()>,
}

impl FrameLease<'_> {
    pub fn descriptor(&self) -> &FrameDescriptor {
        &self.descriptor
    }

    pub fn payload(&self) -> &[u8] {
        // SAFETY: consumer owns this slot until Drop advances tail.
        unsafe { std::slice::from_raw_parts(self.payload, self.descriptor.payload_bytes as usize) }
    }
}

impl Drop for FrameLease<'_> {
    fn drop(&mut self) {
        // Release prevents the producer from reusing the slot before all reads
        // performed through this lease have completed.
        self.consumer
            .mapping
            .header()
            .tail
            .store(self.tail.wrapping_add(1), Ordering::Release);
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn validate_header(header: &RingHeader, map_len: usize) -> Result<()> {
    ensure!(header.magic == RING_MAGIC, "invalid media ring magic");
    ensure!(
        header.abi_version == WIRELESS_AUDIO_ABI_VERSION,
        "media ring ABI mismatch"
    );
    ensure!(
        header.header_bytes as usize == size_of::<RingHeader>(),
        "media ring header size mismatch"
    );
    ensure!(header.slot_count >= 2, "invalid media ring slot count");
    ensure!(
        (header.slot_stride as usize).is_multiple_of(CACHE_LINE),
        "invalid media ring alignment"
    );
    let expected =
        size_of::<RingHeader>() + header.slot_stride as usize * header.slot_count as usize;
    ensure!(expected == map_len, "media ring mapping length mismatch");
    Ok(())
}

fn create_memfd(name: &str, len: usize) -> io::Result<OwnedFd> {
    let name = std::ffi::CString::new(name).expect("static memfd name");
    // SAFETY: syscall arguments follow memfd_create(2), CString is terminated.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as i32
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just returned uniquely by the kernel.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(owned.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Shape is immutable after publication, but writes through existing shared
    // mappings remain legal for the ring slots and cursor cache lines.
    let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(owned)
}

fn fd_len(fd: RawFd) -> io::Result<usize> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes the complete stat structure on success.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    usize::try_from(stat.st_size).map_err(|_| io::Error::other("negative memfd size"))
}

fn create_eventfd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd has no pointer arguments and returns a fresh descriptor.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn duplicate_fd(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl duplicates an existing valid descriptor.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

fn signal_eventfd(fd: RawFd) -> io::Result<()> {
    let value = 1_u64;
    // SAFETY: eventfd accepts exactly one native u64.
    let result = unsafe { libc::write(fd, (&value as *const u64).cast(), size_of::<u64>()) };
    if result == size_of::<u64>() as isize {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // A saturated eventfd does not invalidate a ring publication; its cursor
    // is still visible and a prior wakeup is already pending.
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireless::{PayloadKind, types::IsoStatus};

    #[tokio::test]
    async fn slot_lifetime_applies_backpressure_without_copying_on_read() {
        let (mut producer, mut consumer) = RingPair::create(2, 64).unwrap();
        let mut descriptor = FrameDescriptor::new(PayloadKind::Lc3Sdu, 7);
        descriptor.iso_status = IsoStatus::PossiblyInvalid as u8;
        assert!(producer.try_push_copy(descriptor, b"LC3-SDU").unwrap());

        let lease = consumer.try_acquire().unwrap().unwrap();
        assert_eq!(lease.descriptor().sequence, 7);
        assert_eq!(lease.payload(), b"LC3-SDU");
        drop(lease);
        assert_eq!(consumer.queued_frames(), 0);
    }

    #[tokio::test]
    async fn reservation_writes_directly_into_shared_slot() {
        let (mut producer, mut consumer) = RingPair::create(2, 32).unwrap();
        let mut reservation = producer.try_reserve().unwrap();
        reservation.payload_mut()[..4].copy_from_slice(&[1, 2, 3, 4]);
        reservation
            .commit(FrameDescriptor::new(PayloadKind::Pcm, 1), 4)
            .unwrap();
        let lease = consumer.try_acquire().unwrap().unwrap();
        assert_eq!(lease.payload(), &[1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn eventfd_wakes_an_async_consumer() {
        let (mut producer, mut consumer) = RingPair::create(2, 16).unwrap();
        producer
            .try_push_copy(FrameDescriptor::new(PayloadKind::Pcm, 9), &[8, 7])
            .unwrap();
        consumer.wait_readable().await.unwrap();
        let lease = consumer.try_acquire().unwrap().unwrap();
        assert_eq!(lease.descriptor().sequence, 9);
        assert_eq!(lease.payload(), &[8, 7]);
    }

    #[tokio::test]
    async fn endpoints_survive_linear_descriptor_transfer() {
        let (producer, consumer) = RingPair::create(2, 16).unwrap();
        let mut producer = Producer::from_descriptor(producer.into_descriptor()).unwrap();
        let mut consumer = Consumer::from_descriptor(consumer.into_descriptor()).unwrap();
        producer
            .try_push_copy(FrameDescriptor::new(PayloadKind::Lc3Sdu, 42), b"sdu")
            .unwrap();
        consumer.wait_readable().await.unwrap();
        let lease = consumer.try_acquire().unwrap().unwrap();
        assert_eq!(lease.descriptor().sequence, 42);
        assert_eq!(lease.payload(), b"sdu");
    }
}

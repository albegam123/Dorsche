#include "dorsche_core.h"

#include "dorsche/src/bridge.rs.h"

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <cmath>
#include <cstring>
#include <stdexcept>
#include <string>
#include <utility>

#include <fcntl.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#if __has_include(<linux/dma-heap.h>)
#include <linux/dma-heap.h>
#define DORSCHE_HAS_DMA_HEAP 1
#else
#define DORSCHE_HAS_DMA_HEAP 0
#endif

#if __has_include(<linux/dma-buf.h>)
#include <linux/dma-buf.h>
#define DORSCHE_HAS_DMA_BUF_SYNC 1
#else
#define DORSCHE_HAS_DMA_BUF_SYNC 0
#endif

#if __has_include(<bluetooth/bluetooth.h>) && __has_include(<bluetooth/hci.h>)
#include <bluetooth/bluetooth.h>
#include <bluetooth/hci.h>
#define DORSCHE_HAS_RAW_HCI 1
#else
#define DORSCHE_HAS_RAW_HCI 0
#endif

#if __has_include(<linux/memfd.h>)
#include <linux/memfd.h>
#endif

namespace dorsche::ffi {
namespace {

constexpr double kPi = 3.14159265358979323846;

std::uint64_t monotonic_time_ns() noexcept {
  timespec ts{};
  ::clock_gettime(CLOCK_MONOTONIC, &ts);
  return static_cast<std::uint64_t>(ts.tv_sec) * 1'000'000'000ULL +
         static_cast<std::uint64_t>(ts.tv_nsec);
}

int create_memfd(std::size_t byte_count) {
#ifdef SYS_memfd_create
  const int fd = static_cast<int>(
      ::syscall(SYS_memfd_create, "dorsche-pcm", MFD_CLOEXEC));
  if (fd < 0) return -1;
  if (::ftruncate(fd, static_cast<off_t>(byte_count)) < 0) {
    ::close(fd);
    return -1;
  }
  return fd;
#else
  (void)byte_count;
  return -1;
#endif
}

std::pair<int, bool> allocate_shared_audio(std::size_t byte_count) {
#if DORSCHE_HAS_DMA_HEAP
  // dma_heap allocation returns a genuine DMA-BUF fd. No CPU-side audio copy
  // occurs when this fd is later mmap'ed in Rust; both mappings name the same
  // physical pages. CI and desktop machines normally take the memfd fallback.
  const int heap = ::open("/dev/dma_heap/system", O_RDWR | O_CLOEXEC);
  if (heap >= 0) {
    dma_heap_allocation_data allocation{};
    allocation.len = byte_count;
    allocation.fd_flags = O_RDWR | O_CLOEXEC;
    if (::ioctl(heap, DMA_HEAP_IOCTL_ALLOC, &allocation) == 0) {
      ::close(heap);
      return {static_cast<int>(allocation.fd), true};
    }
    ::close(heap);
  }
#endif
  const int fd = create_memfd(byte_count);
  if (fd < 0) {
    throw std::runtime_error("unable to allocate DMA-BUF or memfd: " +
                             std::string(std::strerror(errno)));
  }
  return {fd, false};
}

void dma_buf_cpu_sync(int fd, std::uint64_t flags) noexcept {
#if DORSCHE_HAS_DMA_BUF_SYNC
  dma_buf_sync sync{flags};
  (void)::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
#else
  (void)fd;
  (void)flags;
#endif
}

}  // namespace

HardwareFrame::HardwareFrame(std::size_t sample_count,
                             std::uint32_t sample_rate,
                             std::uint64_t sequence)
    : sample_count_(sample_count),
      byte_count_(sample_count * sizeof(float)),
      sample_rate_(sample_rate),
      sequence_(sequence),
      capture_time_ns_(monotonic_time_ns()) {
  auto [fd, dma_buf] = allocate_shared_audio(byte_count_);
  buffer_fd_ = fd;
  is_dma_buf_ = dma_buf;

  void* mapping = ::mmap(nullptr, byte_count_, PROT_READ | PROT_WRITE,
                         MAP_SHARED, buffer_fd_, 0);
  if (mapping == MAP_FAILED) {
    ::close(buffer_fd_);
    buffer_fd_ = -1;
    throw std::runtime_error("mmap capture buffer failed");
  }
  writable_mapping_ = static_cast<float*>(mapping);

#if DORSCHE_HAS_DMA_BUF_SYNC
  if (is_dma_buf_) {
    dma_buf_cpu_sync(buffer_fd_, DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE);
  }
#endif

  // Synthetic 10 ms microphone input. A real backend replaces only this fill
  // loop with ALSA/HCI/USB dequeue; the topshim and ownership protocol remain.
  const bool speech_burst = (sequence_ / 80U) % 2U == 1U;
  const float amplitude = speech_burst ? 0.28F : 0.025F;
  for (std::size_t i = 0; i < sample_count_; ++i) {
    const double phase = 2.0 * kPi * 440.0 *
                         static_cast<double>(sequence_ * sample_count_ + i) /
                         static_cast<double>(sample_rate_);
    writable_mapping_[i] = amplitude * static_cast<float>(std::sin(phase));
  }

#if DORSCHE_HAS_DMA_BUF_SYNC
  if (is_dma_buf_) {
    dma_buf_cpu_sync(buffer_fd_, DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE);
  }
#endif

  // eventfd models a pollable completion fence. Production drivers should put
  // their sync_file fd here. Release-before-signal pairs with Rust's fence wait
  // and makes every PCM store visible before the consumer maps/reads the page.
  std::atomic_thread_fence(std::memory_order_release);
  fence_fd_ = ::eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (fence_fd_ < 0) {
    ::munmap(writable_mapping_, byte_count_);
    writable_mapping_ = nullptr;
    ::close(buffer_fd_);
    buffer_fd_ = -1;
    throw std::runtime_error("eventfd fence creation failed");
  }
  const std::uint64_t signaled = 1;
  if (::write(fence_fd_, &signaled, sizeof(signaled)) != sizeof(signaled)) {
    ::close(fence_fd_);
    fence_fd_ = -1;
    ::munmap(writable_mapping_, byte_count_);
    writable_mapping_ = nullptr;
    ::close(buffer_fd_);
    buffer_fd_ = -1;
    throw std::runtime_error("eventfd fence signal failed");
  }
}

HardwareFrame::~HardwareFrame() {
  if (writable_mapping_ != nullptr) {
    ::munmap(writable_mapping_, byte_count_);
  }
  if (buffer_fd_ >= 0) ::close(buffer_fd_);
  if (fence_fd_ >= 0) ::close(fence_fd_);
}

std::size_t HardwareFrame::sample_count() const noexcept {
  return sample_count_;
}
std::uint32_t HardwareFrame::sample_rate() const noexcept {
  return sample_rate_;
}
std::uint64_t HardwareFrame::sequence() const noexcept { return sequence_; }
std::uint64_t HardwareFrame::capture_time_ns() const noexcept {
  return capture_time_ns_;
}
bool HardwareFrame::is_dma_buf() const noexcept { return is_dma_buf_; }

std::int32_t HardwareFrame::take_buffer_fd() noexcept {
  // End C++ CPU access before fd ownership crosses the language boundary.
  // The underlying pages survive because the fd is still open.
  if (writable_mapping_ != nullptr) {
    ::munmap(writable_mapping_, byte_count_);
    writable_mapping_ = nullptr;
  }
  return std::exchange(buffer_fd_, -1);
}

std::int32_t HardwareFrame::take_fence_fd() noexcept {
  return std::exchange(fence_fd_, -1);
}

HardwareController::HardwareController() {
#if DORSCHE_HAS_RAW_HCI
  // Floss-style user-space ownership begins with the raw HCI control channel.
  // Failure is non-fatal so the synthetic/audio-only backend runs in CI.
  hci_control_fd_ =
      ::socket(AF_BLUETOOTH, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, BTPROTO_HCI);
  if (hci_control_fd_ >= 0) {
    sockaddr_hci address{};
    address.hci_family = AF_BLUETOOTH;
    address.hci_dev = HCI_DEV_NONE;
    address.hci_channel = HCI_CHANNEL_CONTROL;
    if (::bind(hci_control_fd_, reinterpret_cast<sockaddr*>(&address),
               sizeof(address)) < 0) {
      ::close(hci_control_fd_);
      hci_control_fd_ = -1;
    }
  }
#endif
}

HardwareController::~HardwareController() {
  if (hci_control_fd_ >= 0) ::close(hci_control_fd_);
}

std::unique_ptr<HardwareFrame> HardwareController::capture_next() {
  constexpr std::size_t kSamplesPerTenMs = 160;
  constexpr std::uint32_t kSampleRate = 16'000;
  return std::make_unique<HardwareFrame>(kSamplesPerTenMs, kSampleRate,
                                         capture_sequence_++);
}

PlaybackReceipt HardwareController::play_pcm(
    rust::Slice<const float> samples, std::uint64_t sequence) noexcept {
  // Placeholder for an ALSA/PipeWire-style queue ioctl. Iterating here merely
  // touches the borrowed pages; it never allocates or copies the PCM payload.
  float peak = 0.0F;
  for (const float sample : samples) peak = std::max(peak, std::abs(sample));
  (void)peak;
  return PlaybackReceipt{sequence, monotonic_time_ns()};
}

std::unique_ptr<HardwareController> new_hardware_controller() {
  return std::make_unique<HardwareController>();
}

}  // namespace dorsche::ffi

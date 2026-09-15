#pragma once

#include "rust/cxx.h"

#include <cstddef>
#include <cstdint>
#include <memory>

namespace dorsche::ffi {

struct PlaybackReceipt;

class HardwareFrame final {
 public:
  HardwareFrame(std::size_t sample_count, std::uint32_t sample_rate,
                std::uint64_t sequence);
  ~HardwareFrame();

  HardwareFrame(const HardwareFrame&) = delete;
  HardwareFrame& operator=(const HardwareFrame&) = delete;
  HardwareFrame(HardwareFrame&&) = delete;
  HardwareFrame& operator=(HardwareFrame&&) = delete;

  std::size_t sample_count() const noexcept;
  std::uint32_t sample_rate() const noexcept;
  std::uint64_t sequence() const noexcept;
  std::uint64_t capture_time_ns() const noexcept;
  bool is_dma_buf() const noexcept;

  std::int32_t take_buffer_fd() noexcept;
  std::int32_t take_fence_fd() noexcept;

 private:
  std::size_t sample_count_{};
  std::size_t byte_count_{};
  std::uint32_t sample_rate_{};
  std::uint64_t sequence_{};
  std::uint64_t capture_time_ns_{};
  std::int32_t buffer_fd_{-1};
  std::int32_t fence_fd_{-1};
  float* writable_mapping_{nullptr};
  bool is_dma_buf_{false};
};

class HardwareController final {
 public:
  HardwareController();
  ~HardwareController();

  HardwareController(const HardwareController&) = delete;
  HardwareController& operator=(const HardwareController&) = delete;

  std::unique_ptr<HardwareFrame> capture_next();
  PlaybackReceipt play_pcm(rust::Slice<const float> samples,
                           std::uint64_t sequence) noexcept;

 private:
  std::uint64_t capture_sequence_{0};
  std::int32_t hci_control_fd_{-1};
};

std::unique_ptr<HardwareController> new_hardware_controller();

}  // namespace dorsche::ffi

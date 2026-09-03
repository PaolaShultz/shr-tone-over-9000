#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstring>
#include <filesystem>
#include <limits>
#include <memory>
#include <new>
#include <stdexcept>
#include <string>

#include "NAM/dsp.h"
#include "NAM/get_dsp.h"

namespace
{
struct NamHandle
{
  std::unique_ptr<nam::DSP> dsp;
  std::string architecture;
  std::string version;
  double expected_sample_rate = -1.0;
  double input_level_dbu = std::numeric_limits<double>::quiet_NaN();
  double output_level_dbu = std::numeric_limits<double>::quiet_NaN();
  std::size_t weight_count = 0;
};

void copy_text(const std::string& source, char* destination, std::size_t capacity)
{
  if (destination == nullptr || capacity == 0)
    return;
  const auto count = std::min(source.size(), capacity - 1);
  std::memcpy(destination, source.data(), count);
  destination[count] = '\0';
}

double optional_number(const nlohmann::json& metadata, const char* key)
{
  const auto value = metadata.find(key);
  if (value == metadata.end() || value->is_null() || !value->is_number())
    return std::numeric_limits<double>::quiet_NaN();
  return value->get<double>();
}
} // namespace

extern "C" void* nam_model_load(const char* path, unsigned int sample_rate,
                                unsigned int maximum_frames, char* error,
                                std::size_t error_capacity)
{
  try
  {
    if (path == nullptr || path[0] == '\0')
      throw std::runtime_error("NAM path is empty");
    if (sample_rate != 48000)
      throw std::runtime_error("v1 requires a 48000 Hz JACK server");
    if (maximum_frames == 0)
      throw std::runtime_error("JACK period must be greater than zero");

    nam::dspData data;
    nam::DspLoadOptions options;
    options.prewarm = false;
    auto dsp = nam::get_dsp(std::filesystem::path(path), data, options);
    if (!dsp)
      throw std::runtime_error("NeuralAmpModelerCore returned no model");
    if (dsp->NumInputChannels() != 1 || dsp->NumOutputChannels() != 1)
      throw std::runtime_error("v1 accepts mono-input, mono-output NAM models only");
    if (data.expected_sample_rate > 0.0
        && std::abs(data.expected_sample_rate - static_cast<double>(sample_rate)) > 0.5)
      throw std::runtime_error("NAM expected sample rate does not match JACK's 48000 Hz");

    // Reset and prewarm happen on the caller's non-real-time loading thread.
    // The JACK process callback only calls process().
    dsp->Reset(static_cast<double>(sample_rate), static_cast<int>(maximum_frames));

    auto handle = std::make_unique<NamHandle>();
    handle->architecture = data.architecture;
    handle->version = data.version;
    handle->expected_sample_rate = data.expected_sample_rate;
    handle->input_level_dbu = optional_number(data.metadata, "input_level_dbu");
    handle->output_level_dbu = optional_number(data.metadata, "output_level_dbu");
    handle->weight_count = data.weights.size();
    handle->dsp = std::move(dsp);
    copy_text("", error, error_capacity);
    return handle.release();
  }
  catch (const std::exception& exception)
  {
    copy_text(exception.what(), error, error_capacity);
  }
  catch (...)
  {
    copy_text("unknown NeuralAmpModelerCore failure", error, error_capacity);
  }
  return nullptr;
}

extern "C" void nam_model_free(void* opaque)
{
  delete static_cast<NamHandle*>(opaque);
}

extern "C" bool nam_model_process(void* opaque, const float* input, float* output,
                                  std::size_t frames)
{
  auto* handle = static_cast<NamHandle*>(opaque);
  if (handle == nullptr || input == nullptr || output == nullptr)
    return false;
  try
  {
    auto* input_channel = const_cast<float*>(input);
    auto* output_channel = output;
    handle->dsp->process(&input_channel, &output_channel, static_cast<int>(frames));
    return true;
  }
  catch (...)
  {
    std::fill(output, output + frames, 0.0F);
    return false;
  }
}

extern "C" double nam_model_expected_sample_rate(const void* opaque)
{
  return static_cast<const NamHandle*>(opaque)->expected_sample_rate;
}

extern "C" double nam_model_input_level_dbu(const void* opaque)
{
  return static_cast<const NamHandle*>(opaque)->input_level_dbu;
}

extern "C" double nam_model_output_level_dbu(const void* opaque)
{
  return static_cast<const NamHandle*>(opaque)->output_level_dbu;
}

extern "C" std::size_t nam_model_weight_count(const void* opaque)
{
  return static_cast<const NamHandle*>(opaque)->weight_count;
}

extern "C" void nam_model_architecture(const void* opaque, char* output,
                                       std::size_t capacity)
{
  copy_text(static_cast<const NamHandle*>(opaque)->architecture, output, capacity);
}

extern "C" void nam_model_version(const void* opaque, char* output,
                                  std::size_t capacity)
{
  copy_text(static_cast<const NamHandle*>(opaque)->version, output, capacity);
}

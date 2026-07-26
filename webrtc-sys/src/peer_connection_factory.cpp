/*
 * Copyright 2025 LiveKit, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "livekit/peer_connection_factory.h"

#include <algorithm>
#include <cstdlib>
#include <memory>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include "api/audio/audio_device.h"
#include "api/audio/builtin_audio_processing_builder.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/audio_options.h"
#include "api/create_modular_peer_connection_factory.h"
#include "api/enable_media.h"
#include "api/environment/environment_factory.h"
#include "api/fec_controller.h"
#include "api/field_trials_view.h"
#include "api/peer_connection_interface.h"
#include "api/rtc_error.h"
#include "api/rtc_event_log/rtc_event_log_factory.h"
#include "api/task_queue/default_task_queue_factory.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "livekit/adm_proxy.h"
#include "livekit/audio_track.h"
#include "livekit/peer_connection.h"
#include "livekit/rtc_error.h"
#include "livekit/rtp_parameters.h"
#include "livekit/video_decoder_factory.h"
#include "livekit/video_encoder_factory.h"
#include "livekit/webrtc.h"
#include "modules/video_coding/fec_controller_default.h"
#include "rtc_base/checks.h"
#include "rtc_base/thread.h"
#include "webrtc-sys/src/peer_connection.rs.h"
#include "webrtc-sys/src/peer_connection_factory.rs.h"

namespace livekit_ffi {
namespace {

constexpr char kForcePlayoutDelayFieldTrial[] =
    "WebRTC-ForcePlayoutDelay/min_ms:0,max_ms:0/";
constexpr char kForcePlayoutDelayValue[] = "min_ms:0,max_ms:0";

// SimKit patch: inject field trials without calling webrtc::FieldTrials.
// The public headers declare FieldTrials::Create, but the official prebuilt
// archives used by non-macOS consumers do not export that implementation.
// Keeping the immutable view in this wrapper makes the same source link
// against both the official archives and SimKit's pinned macOS archive.
class InjectedFieldTrials final : public webrtc::FieldTrialsView {
 public:
  InjectedFieldTrials(const InjectedFieldTrials&) = default;

  static std::unique_ptr<InjectedFieldTrials> Create(
      const std::string& serialized) {
    auto result =
        std::unique_ptr<InjectedFieldTrials>(new InjectedFieldTrials());
    if (!result->Parse(serialized)) {
      return nullptr;
    }
    return result;
  }

  std::string Lookup(absl::string_view key) const override {
    const auto found = values_.find(std::string(key.data(), key.size()));
    return found == values_.end() ? std::string() : found->second;
  }

  std::unique_ptr<webrtc::FieldTrialsView> CreateCopy() const override {
    return std::make_unique<InjectedFieldTrials>(*this);
  }

  void Set(std::string trial, std::string group) {
    RTC_CHECK(!trial.empty());
    RTC_CHECK(!group.empty());
    values_.insert_or_assign(std::move(trial), std::move(group));
  }

 private:
  InjectedFieldTrials() = default;

  bool Parse(const std::string& serialized) {
    if (serialized.empty()) {
      return true;
    }
    if (serialized.back() != '/') {
      return false;
    }

    size_t cursor = 0;
    while (cursor < serialized.size()) {
      const size_t trial_end = serialized.find('/', cursor);
      if (trial_end == std::string::npos || trial_end == cursor) {
        return false;
      }
      const size_t group_start = trial_end + 1;
      const size_t group_end = serialized.find('/', group_start);
      if (group_end == std::string::npos || group_end == group_start) {
        return false;
      }

      std::string trial = serialized.substr(cursor, trial_end - cursor);
      std::string group =
          serialized.substr(group_start, group_end - group_start);
      if (values_.find(trial) != values_.end()) {
        return false;
      }
      values_.emplace(std::move(trial), std::move(group));
      cursor = group_end + 1;
    }
    return true;
  }

  std::unordered_map<std::string, std::string> values_;
};

// Honor WEBRTC_FIELD_TRIALS in the factory Environment. Invalid input is a
// configuration error and terminates factory creation instead of silently
// falling back to default media behavior. The explicit low-latency and WARP
// settings are applied after parsing and therefore intentionally override the
// same keys.
//
// enable_warp turns on SPED (DTLS-in-STUN) via the WebRTC-IceHandshakeDtls
// field trial. SNAP (SCTP-INIT-in-SDP) is intentionally NOT set here: it maps
// to the immutable enable_sctp_snap RTCConfiguration field, so it must be
// carried on the RtcConfiguration (see RtcConfiguration.enable_sctp_snap) to
// stay consistent across create + set_configuration. Enabling it via a field
// trial makes set_configuration fail ("Modifying the configuration in an
// unsupported way").
webrtc::Environment CreateEnvironmentFromEnvVar(bool zero_playout_delay,
                                                bool enable_warp) {
  webrtc::EnvironmentFactory factory;
  std::string serialized_trials;
  if (const char* env_trials = std::getenv("WEBRTC_FIELD_TRIALS")) {
    serialized_trials = env_trials;
  }
  if (serialized_trials.empty() && !zero_playout_delay && !enable_warp) {
    return factory.Create();
  }

  auto field_trials = InjectedFieldTrials::Create(serialized_trials);
  RTC_CHECK(field_trials != nullptr)
      << "WEBRTC_FIELD_TRIALS has invalid key/group serialization";
  if (zero_playout_delay) {
    field_trials->Set("WebRTC-ForcePlayoutDelay", kForcePlayoutDelayValue);
  }
  if (enable_warp) {
    field_trials->Set("WebRTC-IceHandshakeDtls", "Enabled");
  }
  factory.Set(
      std::unique_ptr<const webrtc::FieldTrialsView>(std::move(field_trials)));
  return factory.Create();
}

// SimKit patch: proactive FEC. The default FEC controller only spends
// redundancy once RTCP receiver reports come back lossy, which leaves the
// first ~1-2s of every loss burst unprotected — exactly the window that
// turns into a PLI keyframe round on links without NACK. This wrapper
// floors the loss estimate fed into the FEC rate tables so a baseline
// redundancy stream flows even while the link looks clean; real loss above
// the floor still raises protection as usual.
class FloorLossFecController : public webrtc::FecController {
 public:
  FloorLossFecController(const webrtc::Environment& env, uint8_t floor_fraction)
      : inner_(env), floor_fraction_(floor_fraction) {}

  void SetProtectionCallback(
      webrtc::VCMProtectionCallback* protection_callback) override {
    inner_.SetProtectionCallback(protection_callback);
  }
  void SetProtectionMethod(bool enable_fec, bool enable_nack) override {
    inner_.SetProtectionMethod(enable_fec, enable_nack);
  }
  void SetEncodingData(size_t width,
                       size_t height,
                       size_t num_temporal_layers,
                       size_t max_payload_size) override {
    inner_.SetEncodingData(width, height, num_temporal_layers,
                           max_payload_size);
  }
  uint32_t UpdateFecRates(uint32_t estimated_bitrate_bps,
                          int actual_framerate,
                          uint8_t fraction_lost,
                          std::vector<bool> loss_mask_vector,
                          int64_t round_trip_time_ms) override {
    return inner_.UpdateFecRates(estimated_bitrate_bps, actual_framerate,
                                 std::max(fraction_lost, floor_fraction_),
                                 std::move(loss_mask_vector),
                                 round_trip_time_ms);
  }
  void UpdateWithEncodedData(
      size_t encoded_image_length,
      webrtc::VideoFrameType encoded_image_frametype) override {
    inner_.UpdateWithEncodedData(encoded_image_length, encoded_image_frametype);
  }
  bool UseLossVectorMask() override { return inner_.UseLossVectorMask(); }

 private:
  webrtc::FecControllerDefault inner_;
  const uint8_t floor_fraction_;
};

class FloorLossFecControllerFactory
    : public webrtc::FecControllerFactoryInterface {
 public:
  explicit FloorLossFecControllerFactory(uint8_t floor_fraction)
      : floor_fraction_(floor_fraction) {}
  std::unique_ptr<webrtc::FecController> CreateFecController(
      const webrtc::Environment& env) override {
    return std::make_unique<FloorLossFecController>(env, floor_fraction_);
  }

 private:
  const uint8_t floor_fraction_;
};

// SIMKIT_FEC_MIN_LOSS_PCT (1-100) enables the proactive-FEC controller with
// the given assumed-loss floor in percent; unset/0 keeps stock behavior.
std::unique_ptr<webrtc::FecControllerFactoryInterface>
MaybeCreateFecControllerFactory() {
  const char* pct_str = std::getenv("SIMKIT_FEC_MIN_LOSS_PCT");
  if (pct_str == nullptr || pct_str[0] == '\0') {
    return nullptr;
  }
  int pct = std::atoi(pct_str);
  if (pct <= 0) {
    return nullptr;
  }
  pct = std::min(pct, 100);
  const uint8_t fraction = static_cast<uint8_t>(pct * 255 / 100);
  return std::make_unique<FloorLossFecControllerFactory>(fraction);
}

}  // namespace

class PeerConnectionObserver;

PeerConnectionFactory::PeerConnectionFactory(
    std::shared_ptr<RtcRuntime> rtc_runtime)
    : PeerConnectionFactory(std::move(rtc_runtime), false, false) {}

PeerConnectionFactory::PeerConnectionFactory(
    std::shared_ptr<RtcRuntime> rtc_runtime,
    bool zero_playout_delay)
    : PeerConnectionFactory(std::move(rtc_runtime), zero_playout_delay, false) {}

PeerConnectionFactory::PeerConnectionFactory(
    std::shared_ptr<RtcRuntime> rtc_runtime,
    bool zero_playout_delay,
    bool enable_warp)
    : rtc_runtime_(rtc_runtime),
      env_(CreateEnvironmentFromEnvVar(zero_playout_delay, enable_warp)) {
  webrtc::PeerConnectionFactoryDependencies dependencies;
  // SimKit patch: hand the field-trial-aware Environment to the factory so
  // every call/stream created from it sees the trials.
  dependencies.env = env_;
  dependencies.network_thread = rtc_runtime_->network_thread();
  dependencies.worker_thread = rtc_runtime_->worker_thread();
  dependencies.signaling_thread = rtc_runtime_->signaling_thread();
  dependencies.socket_factory = rtc_runtime_->network_thread()->socketserver();
  dependencies.event_log_factory =
      std::make_unique<webrtc::RtcEventLogFactory>();
  if (zero_playout_delay) {
    RTC_LOG(LS_INFO) << "WebRTC zero playout delay enabled with field trial: "
                     << kForcePlayoutDelayFieldTrial;
  }
  // SimKit patch: optional proactive-FEC controller (see above).
  dependencies.fec_controller_factory = MaybeCreateFecControllerFactory();

  if (enable_warp) {
    RTC_LOG(LS_INFO) << "WebRTC WARP: SPED enabled via field trial "
                        "WebRTC-IceHandshakeDtls/Enabled/ (SNAP via "
                        "RtcConfiguration.enable_sctp_snap)";
  }

  // Create AdmProxy - it creates and initializes Platform ADM internally
  adm_proxy_ = rtc_runtime_->worker_thread()->BlockingCall([&] {
    return webrtc::make_ref_counted<livekit_ffi::AdmProxy>(
        env_, rtc_runtime_->worker_thread());
  });
  audio_device_ = std::make_shared<AudioDeviceController>(adm_proxy_);

  dependencies.adm = adm_proxy_;

  dependencies.video_encoder_factory =
      std::move(std::make_unique<livekit_ffi::VideoEncoderFactory>());
  dependencies.video_decoder_factory =
      std::move(std::make_unique<livekit_ffi::VideoDecoderFactory>());
  dependencies.audio_encoder_factory =
      webrtc::CreateBuiltinAudioEncoderFactory();
  dependencies.audio_decoder_factory =
      webrtc::CreateBuiltinAudioDecoderFactory();
  dependencies.audio_processing_builder =
      std::make_unique<webrtc::BuiltinAudioProcessingBuilder>();

  webrtc::EnableMedia(dependencies);
  peer_factory_ =
      webrtc::CreateModularPeerConnectionFactory(std::move(dependencies));

  if (peer_factory_.get() == nullptr) {
    RTC_LOG_ERR(LS_ERROR) << "Failed to create PeerConnectionFactory";
    return;
  }
}

PeerConnectionFactory::~PeerConnectionFactory() {
  RTC_LOG(LS_VERBOSE) << "PeerConnectionFactory::~PeerConnectionFactory()";

  peer_factory_ = nullptr;
  audio_device_ = nullptr;
  rtc_runtime_->worker_thread()->BlockingCall([this] { adm_proxy_ = nullptr; });
}

std::shared_ptr<PeerConnection> PeerConnectionFactory::create_peer_connection(
    RtcConfiguration config,
    rust::Box<PeerConnectionObserverWrapper> observer) const {
  std::shared_ptr<PeerConnection> pc = std::make_shared<PeerConnection>(
      rtc_runtime_, peer_factory_, std::move(observer));

  if (!pc->Initialize(to_native_rtc_configuration(config))) {
    throw std::runtime_error(serialize_error(to_error(webrtc::RTCError(
        webrtc::RTCErrorType::INTERNAL_ERROR, "failed to initialize pc"))));
  }

  return pc;
}

std::shared_ptr<VideoTrack> PeerConnectionFactory::create_video_track(
    rust::String label,
    std::shared_ptr<VideoTrackSource> source) const {
  return std::static_pointer_cast<VideoTrack>(
      rtc_runtime_->get_or_create_media_stream_track(
          peer_factory_->CreateVideoTrack(source->get(), label.c_str())));
}

std::shared_ptr<AudioTrack> PeerConnectionFactory::create_audio_track(
    rust::String label,
    std::shared_ptr<AudioTrackSource> source) const {
  return std::static_pointer_cast<AudioTrack>(
      rtc_runtime_->get_or_create_media_stream_track(
          peer_factory_->CreateAudioTrack(label.c_str(), source->get().get())));
}

std::shared_ptr<AudioTrack> PeerConnectionFactory::create_device_audio_track(
    rust::String label) const {
  // Create an audio source that uses the ADM for capture
  webrtc::AudioOptions audio_options;
  audio_options.echo_cancellation = true;
  audio_options.auto_gain_control = true;
  audio_options.noise_suppression = true;

  webrtc::scoped_refptr<webrtc::AudioSourceInterface> audio_source =
      peer_factory_->CreateAudioSource(audio_options);

  if (!audio_source) {
    RTC_LOG(LS_ERROR) << "Failed to create device audio source";
    return nullptr;
  }

  return std::static_pointer_cast<AudioTrack>(
      rtc_runtime_->get_or_create_media_stream_track(
          peer_factory_->CreateAudioTrack(label.c_str(), audio_source.get())));
}

RtpCapabilities PeerConnectionFactory::rtp_sender_capabilities(
    MediaType type) const {
  return to_rust_rtp_capabilities(peer_factory_->GetRtpSenderCapabilities(
      static_cast<webrtc::MediaType>(type)));
}

RtpCapabilities PeerConnectionFactory::rtp_receiver_capabilities(
    MediaType type) const {
  return to_rust_rtp_capabilities(peer_factory_->GetRtpReceiverCapabilities(
      static_cast<webrtc::MediaType>(type)));
}

std::shared_ptr<AudioDeviceController> PeerConnectionFactory::audio_device()
    const {
  return audio_device_;
}

bool PeerConnectionFactory::zero_playout_delay_enabled() const {
  return env_.field_trials().Lookup("WebRTC-ForcePlayoutDelay") ==
         kForcePlayoutDelayValue;
}

std::shared_ptr<PeerConnectionFactory> create_peer_connection_factory() {
  return std::make_shared<PeerConnectionFactory>(RtcRuntime::create());
}

std::shared_ptr<PeerConnectionFactory>
create_peer_connection_factory_with_zero_playout_delay() {
  return std::make_shared<PeerConnectionFactory>(RtcRuntime::create(), true);
}

std::shared_ptr<PeerConnectionFactory>
create_peer_connection_factory_with_options(bool zero_playout_delay,
                                            bool enable_warp) {
  return std::make_shared<PeerConnectionFactory>(RtcRuntime::create(),
                                                 zero_playout_delay,
                                                 enable_warp);
}

}  // namespace livekit_ffi

// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Packet trailer support for end-to-end frame metadata propagation.
//!
//! This module provides functionality to embed user-supplied metadata
//! in encoded video frames as trailers. The timestamps/frameIDs are preserved
//! through the WebRTC pipeline and can be extracted on the receiver side.
//!
//! On the send side, user timestamps/frameIDs are stored in the handler's internal
//! map keyed by RTP timestamp. When the encoder produces a frame,
//! the transformer looks up the metadata via the frame's CaptureTime().
//!
//! On the receive side, extracted frame metadata is stored in an
//! internal map keyed by RTP timestamp. Decoded frames look up their
//! metadata via lookup_frame_metadata(rtp_timestamp).

use std::sync::Arc;

use cxx::SharedPtr;
use thiserror::Error;
use webrtc_sys::packet_trailer::ffi as sys_pt;
use webrtc_sys::webrtc as sys_rtc;

use crate::{
    peer_connection_factory::PeerConnectionFactory, rtp_receiver::RtpReceiver,
    rtp_sender::RtpSender,
};

/// Stage reached by a native local video frame in the publish pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishTimingStage {
    /// The adapted raw frame was handed to WebRTC's encoder path.
    EncoderUpload,
    /// WebRTC produced an encoded frame for packetization.
    EncoderOutput,
    /// The encoded frame was handed back to WebRTC's packetizer.
    WebrtcPacketize,
}

/// Stage reached by a native remote video frame in the subscribe pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeTimingStage {
    /// WebRTC produced an encoded frame after RTP depacketization.
    WebrtcReceive,
    /// The encoded frame was handed to WebRTC's decoder.
    DecoderUpload,
    /// WebRTC produced a decoded frame for the native video sink.
    DecoderOutput,
}

/// Timestamped native local video publish pipeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishTimingEvent {
    /// Publish pipeline stage reached by the frame.
    pub stage: PublishTimingStage,
    /// Wall-clock time when this stage was observed, in microseconds since the Unix epoch.
    pub timestamp_us: u64,
    /// User capture timestamp associated with this frame, in microseconds since the Unix epoch.
    pub capture_timestamp_us: u64,
    /// Optional application frame ID associated with this frame.
    pub frame_id: Option<u32>,
}

/// Timestamped native local video publish event with final RTP identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishTimingEventV2 {
    /// Publish pipeline stage reached by the frame.
    pub stage: PublishTimingStage,
    /// Wall-clock time when this stage was observed, in microseconds since the Unix epoch.
    pub timestamp_us: u64,
    /// User capture timestamp associated with this frame, in microseconds since the Unix epoch.
    pub capture_timestamp_us: u64,
    /// Optional application frame ID associated with this frame.
    pub frame_id: Option<u32>,
    /// Final RTP timestamp assigned by the sender, once packetization starts.
    ///
    /// This is `None` for stages before an RTP timestamp exists.
    pub rtp_timestamp: Option<u32>,
    /// SSRC that owns [`Self::rtp_timestamp`].
    pub ssrc: Option<u32>,
    /// Whether the encoded frame is a keyframe, once encoding completes.
    pub is_keyframe: Option<bool>,
}

/// Timestamped native remote video subscribe pipeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeTimingEvent {
    /// Subscribe pipeline stage reached by the frame.
    pub stage: SubscribeTimingStage,
    /// Wall-clock time when this stage was observed, in microseconds since the Unix epoch.
    pub timestamp_us: u64,
    /// User capture timestamp associated with this frame, in microseconds since the Unix epoch.
    pub capture_timestamp_us: u64,
    /// Optional application frame ID associated with this frame.
    pub frame_id: Option<u32>,
}

/// Callback invoked for native local video publish timing events.
pub type PublishTimingObserver = Arc<dyn Fn(PublishTimingEvent) + Send + Sync + 'static>;
/// Callback invoked for native local video publish timing events with final RTP identity.
pub type PublishTimingObserverV2 = Arc<dyn Fn(PublishTimingEventV2) + Send + Sync + 'static>;
/// Callback invoked for native remote video subscribe timing events.
pub type SubscribeTimingObserver = Arc<dyn Fn(SubscribeTimingEvent) + Send + Sync + 'static>;

/// Error returned when creating a sender-side packet trailer handler.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PacketTrailerSenderError {
    /// Packet trailer publish timing is defined only for video senders.
    #[error("packet trailer sender must be a video RTP sender")]
    NonVideoSender,
    /// The native layer rejected an otherwise valid video sender.
    #[error("native packet trailer sender creation failed")]
    NativeRejected,
}

impl From<sys_pt::VideoPublishTimingStage> for PublishTimingStage {
    fn from(stage: sys_pt::VideoPublishTimingStage) -> Self {
        match stage {
            sys_pt::VideoPublishTimingStage::EncoderUpload => Self::EncoderUpload,
            sys_pt::VideoPublishTimingStage::EncoderOutput => Self::EncoderOutput,
            sys_pt::VideoPublishTimingStage::WebrtcPacketize => Self::WebrtcPacketize,
            _ => Self::WebrtcPacketize,
        }
    }
}

impl From<sys_pt::VideoPublishTimingEvent> for PublishTimingEvent {
    fn from(event: sys_pt::VideoPublishTimingEvent) -> Self {
        Self {
            stage: event.stage.into(),
            timestamp_us: event.timestamp_us,
            capture_timestamp_us: event.capture_timestamp_us,
            frame_id: (event.frame_id != 0).then_some(event.frame_id),
        }
    }
}

impl From<sys_pt::VideoPublishTimingEventV2> for PublishTimingEventV2 {
    fn from(event: sys_pt::VideoPublishTimingEventV2) -> Self {
        Self {
            stage: event.stage.into(),
            timestamp_us: event.timestamp_us,
            capture_timestamp_us: event.capture_timestamp_us,
            frame_id: (event.frame_id != 0).then_some(event.frame_id),
            rtp_timestamp: event.has_rtp_timestamp.then_some(event.rtp_timestamp),
            ssrc: event.has_rtp_timestamp.then_some(event.ssrc),
            is_keyframe: event.has_keyframe.then_some(event.is_keyframe),
        }
    }
}

impl From<sys_pt::VideoSubscribeTimingStage> for SubscribeTimingStage {
    fn from(stage: sys_pt::VideoSubscribeTimingStage) -> Self {
        match stage {
            sys_pt::VideoSubscribeTimingStage::WebrtcReceive => Self::WebrtcReceive,
            sys_pt::VideoSubscribeTimingStage::DecoderUpload => Self::DecoderUpload,
            sys_pt::VideoSubscribeTimingStage::DecoderOutput => Self::DecoderOutput,
            _ => Self::DecoderOutput,
        }
    }
}

impl From<sys_pt::VideoSubscribeTimingEvent> for SubscribeTimingEvent {
    fn from(event: sys_pt::VideoSubscribeTimingEvent) -> Self {
        Self {
            stage: event.stage.into(),
            timestamp_us: event.timestamp_us,
            capture_timestamp_us: event.capture_timestamp_us,
            frame_id: (event.frame_id != 0).then_some(event.frame_id),
        }
    }
}

/// Handler for packet trailer embedding/extraction on RTP streams.
///
/// For sender side: Stores frame metadata keyed by capture timestamp
/// and embeds them as binary payload trailers on encoded frames before they
/// are sent. Use `store_frame_metadata()` to associate metadata with
/// a captured frame.
///
/// For receiver side: Extracts frame metadata from received frames
/// and makes them available for retrieval via `lookup_frame_metadata()`.
#[derive(Clone)]
pub struct PacketTrailerHandler {
    sys_handle: SharedPtr<sys_pt::PacketTrailerHandler>,
}

impl PacketTrailerHandler {
    /// Enable or disable timestamp embedding/extraction.
    pub fn set_enabled(&self, enabled: bool) {
        self.sys_handle.set_enabled(enabled);
    }

    /// Check if timestamp embedding/extraction is enabled.
    pub fn enabled(&self) -> bool {
        self.sys_handle.enabled()
    }

    /// Lookup the frame metadata for a given RTP timestamp (receiver side).
    /// Returns `Some((user_timestamp, frame_id, user_data))` if found,
    /// `None` otherwise. The entry is removed from the map after a
    /// successful lookup.
    pub fn lookup_frame_metadata(&self, rtp_timestamp: u32) -> Option<(u64, u32, Vec<u8>)> {
        let ts = self.sys_handle.lookup_timestamp(rtp_timestamp);
        if ts != u64::MAX {
            let frame_id = self.sys_handle.last_lookup_frame_id();
            let user_data = self.sys_handle.last_lookup_user_data();
            Some((ts, frame_id, user_data))
        } else {
            None
        }
    }

    /// Store frame metadata for a given capture timestamp (sender side).
    ///
    /// The `capture_timestamp_us` must be the TimestampAligner-adjusted
    /// timestamp (as produced by `VideoTrackSource::on_captured_frame`),
    /// NOT the original `timestamp_us` from the VideoFrame. The transformer
    /// looks up the metadata by the frame's `CaptureTime()` which is
    /// derived from the aligned value.
    ///
    /// In normal usage this is called automatically by the C++ layer --
    /// callers should set `user_timestamp` and `frame_id` on the
    /// `VideoFrame` and let `capture_frame` / `on_captured_frame` handle
    /// the rest.
    pub fn store_frame_metadata(
        &self,
        capture_timestamp_us: i64,
        user_timestamp: u64,
        frame_id: u32,
        user_data: &[u8],
    ) {
        self.sys_handle.store_frame_metadata(
            capture_timestamp_us,
            user_timestamp,
            frame_id,
            user_data,
        );
    }

    pub(crate) fn sys_handle(&self) -> SharedPtr<sys_pt::PacketTrailerHandler> {
        self.sys_handle.clone()
    }

    /// Set the callback receiving sender-side publish timing events.
    pub fn set_publish_timing_observer(&self, observer: Option<PublishTimingObserver>) {
        if let Some(observer) = observer {
            self.sys_handle.set_publish_timing_observer(Box::new(
                webrtc_sys::packet_trailer::VideoPublishTimingObserverWrapper::new(Box::new(
                    move |event| observer(event.into()),
                )),
            ));
        } else {
            self.sys_handle.clear_publish_timing_observer();
        }
    }

    /// Set the callback receiving sender-side events with final RTP identity.
    pub fn set_publish_timing_observer_v2(&self, observer: Option<PublishTimingObserverV2>) {
        if let Some(observer) = observer {
            self.sys_handle.set_publish_timing_observer_v2(Box::new(
                webrtc_sys::packet_trailer::VideoPublishTimingObserverV2Wrapper::new(Box::new(
                    move |event| observer(event.into()),
                )),
            ));
        } else {
            self.sys_handle.clear_publish_timing_observer_v2();
        }
    }

    /// Set the callback receiving receiver-side subscribe timing events.
    pub fn set_subscribe_timing_observer(&self, observer: Option<SubscribeTimingObserver>) {
        if let Some(observer) = observer {
            self.sys_handle.set_subscribe_timing_observer(Box::new(
                webrtc_sys::packet_trailer::VideoSubscribeTimingObserverWrapper::new(Box::new(
                    move |event| observer(event.into()),
                )),
            ));
        } else {
            self.sys_handle.clear_subscribe_timing_observer();
        }
    }

    pub(crate) fn emit_subscribe_timing(
        &self,
        stage: SubscribeTimingStage,
        capture_timestamp_us: u64,
        frame_id: u32,
    ) {
        let stage = match stage {
            SubscribeTimingStage::WebrtcReceive => sys_pt::VideoSubscribeTimingStage::WebrtcReceive,
            SubscribeTimingStage::DecoderUpload => sys_pt::VideoSubscribeTimingStage::DecoderUpload,
            SubscribeTimingStage::DecoderOutput => sys_pt::VideoSubscribeTimingStage::DecoderOutput,
        };
        self.sys_handle.emit_subscribe_timing(stage, capture_timestamp_us, frame_id);
    }
}

/// Create a sender-side packet trailer handler.
///
/// This handler will embed frame metadata into encoded frames before
/// they are packetized and sent. Use `store_frame_metadata()` to
/// associate metadata with a captured frame's capture timestamp.
#[deprecated(note = "use try_create_sender_handler to reject non-video senders without panicking")]
pub fn create_sender_handler(
    peer_factory: &PeerConnectionFactory,
    sender: &RtpSender,
) -> PacketTrailerHandler {
    try_create_sender_handler(peer_factory, sender)
        .unwrap_or_else(|error| panic!("create_sender_handler failed: {error}"))
}

/// Creates a sender-side packet trailer handler for a video RTP sender.
///
/// Returns an error before installing a transformer when `sender` is not a
/// video sender.
pub fn try_create_sender_handler(
    peer_factory: &PeerConnectionFactory,
    sender: &RtpSender,
) -> Result<PacketTrailerHandler, PacketTrailerSenderError> {
    if !matches!(sender.handle.sys_handle.media_type(), sys_rtc::ffi::MediaType::Video) {
        return Err(PacketTrailerSenderError::NonVideoSender);
    }

    let sys_handle = sys_pt::new_packet_trailer_sender(
        peer_factory.handle.sys_handle.clone(),
        sender.handle.sys_handle.clone(),
    );
    if sys_handle.is_null() {
        return Err(PacketTrailerSenderError::NativeRejected);
    }

    Ok(PacketTrailerHandler { sys_handle })
}

/// Create a receiver-side packet trailer handler.
///
/// This handler will extract frame metadata from received frames
/// and store them in a map keyed by RTP timestamp. Use
/// `lookup_frame_metadata(rtp_timestamp)` to retrieve the metadata
/// for a specific decoded frame.
pub fn create_receiver_handler(
    peer_factory: &PeerConnectionFactory,
    receiver: &RtpReceiver,
) -> PacketTrailerHandler {
    PacketTrailerHandler {
        sys_handle: sys_pt::new_packet_trailer_receiver(
            peer_factory.handle.sys_handle.clone(),
            receiver.handle.sys_handle.clone(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        sync::Arc,
        time::Duration,
    };

    use livekit_runtime::timeout;
    use tokio::sync::mpsc;

    use super::{
        try_create_sender_handler, PacketTrailerSenderError, PublishTimingEvent,
        PublishTimingEventV2, PublishTimingStage,
    };
    use crate::{
        audio_source::{native::NativeAudioSource, AudioSourceOptions},
        media_stream_track::MediaStreamTrack,
        peer_connection::{AnswerOptions, OfferOptions, PeerConnectionState},
        peer_connection_factory::{
            native::PeerConnectionFactoryExt, PeerConnectionFactory, RtcConfiguration,
        },
        video_frame::{FrameMetadata, I420Buffer, VideoFrame, VideoRotation},
        video_source::{native::NativeVideoSource, VideoResolution},
    };
    use webrtc_sys::packet_trailer::ffi::{
        self as sys_pt, VideoPublishTimingEvent, VideoPublishTimingEventV2, VideoPublishTimingStage,
    };

    #[test]
    fn legacy_publish_timing_event_remains_constructible_with_its_original_fields() {
        let event = PublishTimingEvent {
            stage: PublishTimingStage::EncoderUpload,
            timestamp_us: 20,
            capture_timestamp_us: 10,
            frame_id: Some(7),
        };
        let sys_event = VideoPublishTimingEvent {
            stage: VideoPublishTimingStage::EncoderUpload,
            timestamp_us: 20,
            capture_timestamp_us: 10,
            frame_id: 7,
        };

        assert_eq!(event.frame_id, Some(sys_event.frame_id));
    }

    #[test]
    fn publish_timing_preserves_an_exact_zero_rtp_timestamp() {
        let event = PublishTimingEventV2::from(VideoPublishTimingEventV2 {
            stage: VideoPublishTimingStage::WebrtcPacketize,
            timestamp_us: 20,
            capture_timestamp_us: 10,
            frame_id: 7,
            has_rtp_timestamp: true,
            rtp_timestamp: 0,
            ssrc: 42,
            has_keyframe: true,
            is_keyframe: true,
        });

        assert_eq!(event.stage, PublishTimingStage::WebrtcPacketize);
        assert_eq!(event.rtp_timestamp, Some(0));
        assert_eq!(event.ssrc, Some(42));
        assert_eq!(event.is_keyframe, Some(true));
    }

    #[test]
    fn publish_timing_keeps_pre_packetization_rtp_identity_absent() {
        let event = PublishTimingEventV2::from(VideoPublishTimingEventV2 {
            stage: VideoPublishTimingStage::EncoderUpload,
            timestamp_us: 20,
            capture_timestamp_us: 10,
            frame_id: 7,
            has_rtp_timestamp: false,
            rtp_timestamp: 0,
            ssrc: 0,
            has_keyframe: false,
            is_keyframe: false,
        });

        assert_eq!(event.rtp_timestamp, None);
        assert_eq!(event.ssrc, None);
        assert_eq!(event.is_keyframe, None);
    }

    #[test]
    fn publish_timing_preserves_a_non_keyframe_value() {
        let event = PublishTimingEventV2::from(VideoPublishTimingEventV2 {
            stage: VideoPublishTimingStage::EncoderOutput,
            timestamp_us: 20,
            capture_timestamp_us: 10,
            frame_id: 7,
            has_rtp_timestamp: true,
            rtp_timestamp: 1,
            ssrc: 42,
            has_keyframe: true,
            is_keyframe: false,
        });

        assert_eq!(event.is_keyframe, Some(false));
    }

    #[test]
    fn sender_handler_rejects_audio_before_installing_a_transformer() {
        let factory = PeerConnectionFactory::default();
        let connection =
            factory.create_peer_connection(RtcConfiguration::default()).expect("peer connection");
        let source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 100);
        let track = factory.create_audio_track("audio", source);
        let sender =
            connection.add_track(MediaStreamTrack::from(track), &["audio"]).expect("audio sender");

        assert!(matches!(
            try_create_sender_handler(&factory, &sender),
            Err(PacketTrailerSenderError::NonVideoSender)
        ));

        let native = sys_pt::new_packet_trailer_sender(
            factory.handle.sys_handle.clone(),
            sender.handle.sys_handle.clone(),
        );
        assert!(native.is_null(), "the native layer must also reject audio senders");

        #[allow(deprecated)]
        let legacy =
            catch_unwind(AssertUnwindSafe(|| super::create_sender_handler(&factory, &sender)));
        let panic = match legacy {
            Err(panic) => panic,
            Ok(_) => panic!("legacy API must fail explicitly for audio"),
        };
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(message.contains("packet trailer sender must be a video RTP sender"));

        connection.close();
    }

    #[tokio::test]
    async fn v2_observer_reports_real_transform_send_identity() {
        let factory = PeerConnectionFactory::default();
        let sender_pc =
            factory.create_peer_connection(RtcConfiguration::default()).expect("sender peer");
        let receiver_pc =
            factory.create_peer_connection(RtcConfiguration::default()).expect("receiver peer");

        let source = NativeVideoSource::new_without_keepalive(
            VideoResolution { width: 320, height: 180 },
            true,
        );
        let track = factory.create_video_track("video", source.clone());
        let sender =
            sender_pc.add_track(MediaStreamTrack::from(track), &["video"]).expect("video sender");
        let handler =
            try_create_sender_handler(&factory, &sender).expect("video packet trailer handler");
        handler.set_enabled(false);
        source.set_packet_trailer_handler(handler.clone());

        let (legacy_tx, mut legacy_rx) = mpsc::unbounded_channel();
        handler.set_publish_timing_observer(Some(Arc::new(move |event| {
            if event.stage == PublishTimingStage::WebrtcPacketize {
                let _ = legacy_tx.send(event);
            }
        })));
        let (v2_tx, mut v2_rx) = mpsc::unbounded_channel();
        handler.set_publish_timing_observer_v2(Some(Arc::new(move |event| {
            if event.stage == PublishTimingStage::WebrtcPacketize {
                let _ = v2_tx.send(event);
            }
        })));

        let (sender_ice_tx, mut sender_ice_rx) = mpsc::unbounded_channel();
        sender_pc.on_ice_candidate(Some(Box::new(move |candidate| {
            let _ = sender_ice_tx.send(candidate);
        })));
        let (receiver_ice_tx, mut receiver_ice_rx) = mpsc::unbounded_channel();
        receiver_pc.on_ice_candidate(Some(Box::new(move |candidate| {
            let _ = receiver_ice_tx.send(candidate);
        })));
        let (connected_tx, mut connected_rx) = mpsc::unbounded_channel();
        sender_pc.on_connection_state_change(Some(Box::new(move |state| {
            let _ = connected_tx.send(state);
        })));

        let offer = sender_pc.create_offer(OfferOptions::default()).await.expect("offer");
        sender_pc.set_local_description(offer.clone()).await.expect("local offer");
        receiver_pc.set_remote_description(offer).await.expect("remote offer");
        let answer = receiver_pc.create_answer(AnswerOptions::default()).await.expect("answer");
        receiver_pc.set_local_description(answer.clone()).await.expect("local answer");
        sender_pc.set_remote_description(answer).await.expect("remote answer");

        let sender_ice = timeout(Duration::from_secs(5), sender_ice_rx.recv())
            .await
            .expect("sender ICE candidate timeout")
            .expect("sender ICE candidate channel closed");
        let receiver_ice = timeout(Duration::from_secs(5), receiver_ice_rx.recv())
            .await
            .expect("receiver ICE candidate timeout")
            .expect("receiver ICE candidate channel closed");
        sender_pc.add_ice_candidate(receiver_ice).await.expect("receiver ICE");
        receiver_pc.add_ice_candidate(sender_ice).await.expect("sender ICE");

        loop {
            let state = timeout(Duration::from_secs(5), connected_rx.recv())
                .await
                .expect("peer connection timeout")
                .expect("peer connection state channel closed");
            match state {
                PeerConnectionState::Connected => break,
                PeerConnectionState::Failed | PeerConnectionState::Closed => {
                    panic!("peer connection failed before video send: {state:?}")
                }
                _ => {}
            }
        }

        source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: 123_000,
            frame_metadata: Some(FrameMetadata {
                user_timestamp: Some(987_654),
                frame_id: Some(7),
                user_data: None,
            }),
            buffer: I420Buffer::new(320, 180),
        });

        let event = timeout(Duration::from_secs(5), v2_rx.recv())
            .await
            .expect("V2 packetizer event timeout")
            .expect("V2 observer closed");
        assert_eq!(event.capture_timestamp_us, 987_654);
        assert_eq!(event.frame_id, Some(7));
        assert!(event.rtp_timestamp.is_some());
        assert!(event.ssrc.is_some());
        assert_eq!(event.is_keyframe, Some(true));
        let ssrc = event.ssrc.expect("final SSRC");
        assert!(sender
            .parameters()
            .encodings
            .iter()
            .any(|encoding| encoding.has_ssrc && encoding.ssrc == ssrc));

        let legacy = timeout(Duration::from_secs(5), legacy_rx.recv())
            .await
            .expect("legacy packetizer event timeout")
            .expect("legacy observer closed");
        assert_eq!(legacy.capture_timestamp_us, 987_654);
        assert_eq!(legacy.frame_id, Some(7));

        receiver_pc.close();
        sender_pc.close();
    }
}

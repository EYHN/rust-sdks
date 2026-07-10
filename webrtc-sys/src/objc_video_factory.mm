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

#include "livekit/objc_video_factory.h"

#import <sdk/objc/base/RTCVideoCodecInfo.h>
#import <sdk/objc/components/video_codec/RTCDefaultVideoDecoderFactory.h>
#import <sdk/objc/components/video_codec/RTCDefaultVideoEncoderFactory.h>
#import <sdk/objc/components/video_codec/RTCVideoEncoderFactorySimulcast.h>
#include "sdk/objc/native/api/video_decoder_factory.h"
#include "sdk/objc/native/api/video_encoder_factory.h"

// SimKit patch: also advertise plain H264 High (profile-level-id 64001f).
// The default factory only offers ConstrainedHigh (640c1f), which Chrome
// does not list, so negotiation always fell back to ConstrainedBaseline —
// and VideoToolbox only enables the low-latency rate controller (the
// prerequisite for L1T2 temporal layers) on High-family sessions.
@interface SimKitVideoEncoderFactory : RTC_OBJC_TYPE (RTCDefaultVideoEncoderFactory)
@end

@implementation SimKitVideoEncoderFactory

- (NSArray<RTC_OBJC_TYPE(RTCVideoCodecInfo) *> *)supportedCodecs {
  RTC_OBJC_TYPE(RTCVideoCodecInfo)* high = [[RTC_OBJC_TYPE(RTCVideoCodecInfo) alloc]
      initWithName:@"H264"
        parameters:@{
          @"profile-level-id" : @"64001f",
          @"level-asymmetry-allowed" : @"1",
          @"packetization-mode" : @"1",
        }];
  return [@[ high ] arrayByAddingObjectsFromArray:[super supportedCodecs]];
}

@end

namespace livekit_ffi {

std::unique_ptr<webrtc::VideoEncoderFactory> CreateObjCVideoEncoderFactory() {
  SimKitVideoEncoderFactory* encoderFactory = [[SimKitVideoEncoderFactory alloc] init];
  RTC_OBJC_TYPE(RTCVideoEncoderFactorySimulcast)* simulcastFactory =
      [[RTC_OBJC_TYPE(RTCVideoEncoderFactorySimulcast) alloc] initWithPrimary:encoderFactory fallback:encoderFactory];
  return webrtc::ObjCToNativeVideoEncoderFactory(simulcastFactory);
}

std::unique_ptr<webrtc::VideoDecoderFactory> CreateObjCVideoDecoderFactory() {
  return webrtc::ObjCToNativeVideoDecoderFactory([[RTC_OBJC_TYPE(RTCDefaultVideoDecoderFactory) alloc] init]);
}

}  // namespace livekit_ffi

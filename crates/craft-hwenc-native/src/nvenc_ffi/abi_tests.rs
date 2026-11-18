//! The structures of `abi.rs` against a C compiler's view of `nvEncodeAPI.h` (API 12.1): sizes,
//! alignments, the offsets of the fields the session writes or reads, constants and GUIDs. The
//! expected values were printed by a C program built from NVIDIA's MIT-licensed header with MSVC
//! (Windows x64). They hold on Linux x86_64 as well: every field is a fixed-width integer, an
//! enum, a pointer or a GUID, none is a C `long`, and the bit-fields of each `flags` word are
//! `uint32_t` fields laid out from the least significant bit by both compilers.

#![allow(clippy::unreadable_literal)]

use super::abi::*;
use std::mem::{align_of, offset_of, size_of};

#[test]
fn sizes_and_alignments_match_the_header() {
    for (size, align, expected, name) in [
        (
            size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>(),
            align_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>(),
            (1552, 8),
            "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS",
        ),
        (size_of::<NV_ENC_CAPS_PARAM>(), align_of::<NV_ENC_CAPS_PARAM>(), (256, 4), "NV_ENC_CAPS_PARAM"),
        (size_of::<NV_ENC_QP>(), align_of::<NV_ENC_QP>(), (12, 4), "NV_ENC_QP"),
        (size_of::<NV_ENC_RC_PARAMS>(), align_of::<NV_ENC_RC_PARAMS>(), (128, 4), "NV_ENC_RC_PARAMS"),
        (
            size_of::<NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
            align_of::<NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
            (112, 4),
            "NV_ENC_CONFIG_H264_VUI_PARAMETERS",
        ),
        (size_of::<NV_ENC_CONFIG_H264>(), align_of::<NV_ENC_CONFIG_H264>(), (1792, 8), "NV_ENC_CONFIG_H264"),
        (size_of::<NV_ENC_CONFIG_HEVC>(), align_of::<NV_ENC_CONFIG_HEVC>(), (1560, 8), "NV_ENC_CONFIG_HEVC"),
        (size_of::<NV_ENC_CODEC_CONFIG>(), align_of::<NV_ENC_CODEC_CONFIG>(), (1792, 8), "NV_ENC_CODEC_CONFIG"),
        (size_of::<NV_ENC_CONFIG>(), align_of::<NV_ENC_CONFIG>(), (3584, 8), "NV_ENC_CONFIG"),
        (size_of::<NV_ENC_PRESET_CONFIG>(), align_of::<NV_ENC_PRESET_CONFIG>(), (5128, 8), "NV_ENC_PRESET_CONFIG"),
        (
            size_of::<NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE>(),
            align_of::<NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE>(),
            (16, 4),
            "NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE",
        ),
        (size_of::<NV_ENC_INITIALIZE_PARAMS>(), align_of::<NV_ENC_INITIALIZE_PARAMS>(), (1808, 8), "NV_ENC_INITIALIZE_PARAMS"),
        (size_of::<NV_ENC_CREATE_INPUT_BUFFER>(), align_of::<NV_ENC_CREATE_INPUT_BUFFER>(), (776, 8), "NV_ENC_CREATE_INPUT_BUFFER"),
        (size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>(), align_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>(), (776, 8), "NV_ENC_CREATE_BITSTREAM_BUFFER"),
        (size_of::<NV_ENC_CODEC_PIC_PARAMS>(), align_of::<NV_ENC_CODEC_PIC_PARAMS>(), (1552, 8), "NV_ENC_CODEC_PIC_PARAMS"),
        (size_of::<NV_ENC_PIC_PARAMS>(), align_of::<NV_ENC_PIC_PARAMS>(), (3360, 8), "NV_ENC_PIC_PARAMS"),
        (size_of::<NV_ENC_LOCK_BITSTREAM>(), align_of::<NV_ENC_LOCK_BITSTREAM>(), (1552, 8), "NV_ENC_LOCK_BITSTREAM"),
        (size_of::<NV_ENC_LOCK_INPUT_BUFFER>(), align_of::<NV_ENC_LOCK_INPUT_BUFFER>(), (1544, 8), "NV_ENC_LOCK_INPUT_BUFFER"),
        (size_of::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>(), align_of::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>(), (1544, 8), "NV_ENC_SEQUENCE_PARAM_PAYLOAD"),
        (size_of::<NV_ENCODE_API_FUNCTION_LIST>(), align_of::<NV_ENCODE_API_FUNCTION_LIST>(), (2552, 8), "NV_ENCODE_API_FUNCTION_LIST"),
    ] {
        assert_eq!((size, align), expected, "{name}");
    }
}

#[test]
fn field_offsets_match_the_header() {
    for (offset, expected, name) in [
        (offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, deviceType), 4, "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.deviceType"),
        (offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device), 8, "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.device"),
        (offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion), 24, "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.apiVersion"),
        (offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved2), 1040, "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.reserved2"),
        (offset_of!(NV_ENC_CAPS_PARAM, capsToQuery), 4, "NV_ENC_CAPS_PARAM.capsToQuery"),
        (offset_of!(NV_ENC_RC_PARAMS, rateControlMode), 4, "NV_ENC_RC_PARAMS.rateControlMode"),
        (offset_of!(NV_ENC_RC_PARAMS, averageBitRate), 20, "NV_ENC_RC_PARAMS.averageBitRate"),
        (offset_of!(NV_ENC_RC_PARAMS, maxBitRate), 24, "NV_ENC_RC_PARAMS.maxBitRate"),
        (offset_of!(NV_ENC_RC_PARAMS, vbvBufferSize), 28, "NV_ENC_RC_PARAMS.vbvBufferSize"),
        (offset_of!(NV_ENC_RC_PARAMS, vbvInitialDelay), 32, "NV_ENC_RC_PARAMS.vbvInitialDelay"),
        (offset_of!(NV_ENC_RC_PARAMS, minQP), 40, "NV_ENC_RC_PARAMS.minQP"),
        (offset_of!(NV_ENC_RC_PARAMS, targetQuality), 88, "NV_ENC_RC_PARAMS.targetQuality"),
        (offset_of!(NV_ENC_RC_PARAMS, targetQualityLSB), 89, "NV_ENC_RC_PARAMS.targetQualityLSB"),
        (offset_of!(NV_ENC_RC_PARAMS, lookaheadDepth), 90, "NV_ENC_RC_PARAMS.lookaheadDepth"),
        (offset_of!(NV_ENC_RC_PARAMS, qpMapMode), 96, "NV_ENC_RC_PARAMS.qpMapMode"),
        (offset_of!(NV_ENC_RC_PARAMS, reserved), 112, "NV_ENC_RC_PARAMS.reserved"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoSignalTypePresentFlag), 8, "VUI.videoSignalTypePresentFlag"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFormat), 12, "VUI.videoFormat"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFullRangeFlag), 16, "VUI.videoFullRangeFlag"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourDescriptionPresentFlag), 20, "VUI.colourDescriptionPresentFlag"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourPrimaries), 24, "VUI.colourPrimaries"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, transferCharacteristics), 28, "VUI.transferCharacteristics"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourMatrix), 32, "VUI.colourMatrix"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, timingInfoPresentFlag), 52, "VUI.timingInfoPresentFlag"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, numUnitInTicks), 56, "VUI.numUnitInTicks"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, timeScale), 60, "VUI.timeScale"),
        (offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, reserved), 64, "VUI.reserved"),
        (offset_of!(NV_ENC_CONFIG_H264, level), 4, "NV_ENC_CONFIG_H264.level"),
        (offset_of!(NV_ENC_CONFIG_H264, idrPeriod), 8, "NV_ENC_CONFIG_H264.idrPeriod"),
        (offset_of!(NV_ENC_CONFIG_H264, entropyCodingMode), 44, "NV_ENC_CONFIG_H264.entropyCodingMode"),
        (offset_of!(NV_ENC_CONFIG_H264, maxNumRefFrames), 60, "NV_ENC_CONFIG_H264.maxNumRefFrames"),
        (offset_of!(NV_ENC_CONFIG_H264, h264VUIParameters), 72, "NV_ENC_CONFIG_H264.h264VUIParameters"),
        (offset_of!(NV_ENC_CONFIG_H264, ltrNumFrames), 184, "NV_ENC_CONFIG_H264.ltrNumFrames"),
        (offset_of!(NV_ENC_CONFIG_H264, numRefL1), 208, "NV_ENC_CONFIG_H264.numRefL1"),
        (offset_of!(NV_ENC_CONFIG_H264, reserved1), 212, "NV_ENC_CONFIG_H264.reserved1"),
        (offset_of!(NV_ENC_CONFIG_H264, reserved2), 1280, "NV_ENC_CONFIG_H264.reserved2"),
        (offset_of!(NV_ENC_CONFIG_HEVC, tier), 4, "NV_ENC_CONFIG_HEVC.tier"),
        (offset_of!(NV_ENC_CONFIG_HEVC, minCUSize), 8, "NV_ENC_CONFIG_HEVC.minCUSize"),
        (offset_of!(NV_ENC_CONFIG_HEVC, maxCUSize), 12, "NV_ENC_CONFIG_HEVC.maxCUSize"),
        (offset_of!(NV_ENC_CONFIG_HEVC, flags), 16, "NV_ENC_CONFIG_HEVC.flags"),
        (offset_of!(NV_ENC_CONFIG_HEVC, idrPeriod), 20, "NV_ENC_CONFIG_HEVC.idrPeriod"),
        (offset_of!(NV_ENC_CONFIG_HEVC, maxNumRefFramesInDPB), 32, "NV_ENC_CONFIG_HEVC.maxNumRefFramesInDPB"),
        (offset_of!(NV_ENC_CONFIG_HEVC, hevcVUIParameters), 64, "NV_ENC_CONFIG_HEVC.hevcVUIParameters"),
        (offset_of!(NV_ENC_CONFIG_HEVC, ltrTrustMode), 176, "NV_ENC_CONFIG_HEVC.ltrTrustMode"),
        (offset_of!(NV_ENC_CONFIG_HEVC, reserved1), 192, "NV_ENC_CONFIG_HEVC.reserved1"),
        (offset_of!(NV_ENC_CONFIG_HEVC, reserved2), 1048, "NV_ENC_CONFIG_HEVC.reserved2"),
        (offset_of!(NV_ENC_CODEC_CONFIG, h264Config), 0, "NV_ENC_CODEC_CONFIG.h264Config"),
        (offset_of!(NV_ENC_CODEC_CONFIG, hevcConfig), 0, "NV_ENC_CODEC_CONFIG.hevcConfig"),
        (offset_of!(NV_ENC_CONFIG, profileGUID), 4, "NV_ENC_CONFIG.profileGUID"),
        (offset_of!(NV_ENC_CONFIG, gopLength), 20, "NV_ENC_CONFIG.gopLength"),
        (offset_of!(NV_ENC_CONFIG, frameIntervalP), 24, "NV_ENC_CONFIG.frameIntervalP"),
        (offset_of!(NV_ENC_CONFIG, rcParams), 40, "NV_ENC_CONFIG.rcParams"),
        (offset_of!(NV_ENC_CONFIG, encodeCodecConfig), 168, "NV_ENC_CONFIG.encodeCodecConfig"),
        (offset_of!(NV_ENC_CONFIG, reserved), 1960, "NV_ENC_CONFIG.reserved"),
        (offset_of!(NV_ENC_CONFIG, reserved2), 3072, "NV_ENC_CONFIG.reserved2"),
        (offset_of!(NV_ENC_PRESET_CONFIG, presetCfg), 8, "NV_ENC_PRESET_CONFIG.presetCfg"),
        (offset_of!(NV_ENC_PRESET_CONFIG, reserved1), 3592, "NV_ENC_PRESET_CONFIG.reserved1"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeGUID), 4, "NV_ENC_INITIALIZE_PARAMS.encodeGUID"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, presetGUID), 20, "NV_ENC_INITIALIZE_PARAMS.presetGUID"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeWidth), 36, "NV_ENC_INITIALIZE_PARAMS.encodeWidth"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeHeight), 40, "NV_ENC_INITIALIZE_PARAMS.encodeHeight"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, darWidth), 44, "NV_ENC_INITIALIZE_PARAMS.darWidth"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, darHeight), 48, "NV_ENC_INITIALIZE_PARAMS.darHeight"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, frameRateNum), 52, "NV_ENC_INITIALIZE_PARAMS.frameRateNum"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, frameRateDen), 56, "NV_ENC_INITIALIZE_PARAMS.frameRateDen"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, enablePTD), 64, "NV_ENC_INITIALIZE_PARAMS.enablePTD"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeConfig), 88, "NV_ENC_INITIALIZE_PARAMS.encodeConfig"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, maxMEHintCountsPerBlock), 104, "NV_ENC_INITIALIZE_PARAMS.maxMEHintCountsPerBlock"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, tuningInfo), 136, "NV_ENC_INITIALIZE_PARAMS.tuningInfo"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, bufferFormat), 140, "NV_ENC_INITIALIZE_PARAMS.bufferFormat"),
        (offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved2), 1296, "NV_ENC_INITIALIZE_PARAMS.reserved2"),
        (offset_of!(NV_ENC_CREATE_INPUT_BUFFER, memoryHeap), 12, "NV_ENC_CREATE_INPUT_BUFFER.memoryHeap"),
        (offset_of!(NV_ENC_CREATE_INPUT_BUFFER, bufferFmt), 16, "NV_ENC_CREATE_INPUT_BUFFER.bufferFmt"),
        (offset_of!(NV_ENC_CREATE_INPUT_BUFFER, inputBuffer), 24, "NV_ENC_CREATE_INPUT_BUFFER.inputBuffer"),
        (offset_of!(NV_ENC_CREATE_INPUT_BUFFER, reserved2), 272, "NV_ENC_CREATE_INPUT_BUFFER.reserved2"),
        (offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, size), 4, "NV_ENC_CREATE_BITSTREAM_BUFFER.size"),
        (offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, memoryHeap), 8, "NV_ENC_CREATE_BITSTREAM_BUFFER.memoryHeap"),
        (offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer), 16, "NV_ENC_CREATE_BITSTREAM_BUFFER.bitstreamBuffer"),
        (offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved2), 264, "NV_ENC_CREATE_BITSTREAM_BUFFER.reserved2"),
        (offset_of!(NV_ENC_PIC_PARAMS, inputWidth), 4, "NV_ENC_PIC_PARAMS.inputWidth"),
        (offset_of!(NV_ENC_PIC_PARAMS, inputHeight), 8, "NV_ENC_PIC_PARAMS.inputHeight"),
        (offset_of!(NV_ENC_PIC_PARAMS, inputPitch), 12, "NV_ENC_PIC_PARAMS.inputPitch"),
        (offset_of!(NV_ENC_PIC_PARAMS, encodePicFlags), 16, "NV_ENC_PIC_PARAMS.encodePicFlags"),
        (offset_of!(NV_ENC_PIC_PARAMS, inputTimeStamp), 24, "NV_ENC_PIC_PARAMS.inputTimeStamp"),
        (offset_of!(NV_ENC_PIC_PARAMS, inputDuration), 32, "NV_ENC_PIC_PARAMS.inputDuration"),
        (offset_of!(NV_ENC_PIC_PARAMS, inputBuffer), 40, "NV_ENC_PIC_PARAMS.inputBuffer"),
        (offset_of!(NV_ENC_PIC_PARAMS, outputBitstream), 48, "NV_ENC_PIC_PARAMS.outputBitstream"),
        (offset_of!(NV_ENC_PIC_PARAMS, bufferFmt), 64, "NV_ENC_PIC_PARAMS.bufferFmt"),
        (offset_of!(NV_ENC_PIC_PARAMS, pictureStruct), 68, "NV_ENC_PIC_PARAMS.pictureStruct"),
        (offset_of!(NV_ENC_PIC_PARAMS, codecPicParams), 80, "NV_ENC_PIC_PARAMS.codecPicParams"),
        (offset_of!(NV_ENC_PIC_PARAMS, meHintCountsPerBlock), 1632, "NV_ENC_PIC_PARAMS.meHintCountsPerBlock"),
        (offset_of!(NV_ENC_PIC_PARAMS, qpDeltaMap), 1712, "NV_ENC_PIC_PARAMS.qpDeltaMap"),
        (offset_of!(NV_ENC_PIC_PARAMS, outputReconBuffer), 1760, "NV_ENC_PIC_PARAMS.outputReconBuffer"),
        (offset_of!(NV_ENC_PIC_PARAMS, reserved4), 2904, "NV_ENC_PIC_PARAMS.reserved4"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, outputBitstream), 8, "NV_ENC_LOCK_BITSTREAM.outputBitstream"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes), 36, "NV_ENC_LOCK_BITSTREAM.bitstreamSizeInBytes"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, outputTimeStamp), 40, "NV_ENC_LOCK_BITSTREAM.outputTimeStamp"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr), 56, "NV_ENC_LOCK_BITSTREAM.bitstreamBufferPtr"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, pictureType), 64, "NV_ENC_LOCK_BITSTREAM.pictureType"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, outputStatsPtr), 120, "NV_ENC_LOCK_BITSTREAM.outputStatsPtr"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, frameIdxDisplay), 128, "NV_ENC_LOCK_BITSTREAM.frameIdxDisplay"),
        (offset_of!(NV_ENC_LOCK_BITSTREAM, reservedInternal), 1520, "NV_ENC_LOCK_BITSTREAM.reservedInternal"),
        (offset_of!(NV_ENC_LOCK_INPUT_BUFFER, inputBuffer), 8, "NV_ENC_LOCK_INPUT_BUFFER.inputBuffer"),
        (offset_of!(NV_ENC_LOCK_INPUT_BUFFER, bufferDataPtr), 16, "NV_ENC_LOCK_INPUT_BUFFER.bufferDataPtr"),
        (offset_of!(NV_ENC_LOCK_INPUT_BUFFER, pitch), 24, "NV_ENC_LOCK_INPUT_BUFFER.pitch"),
        (offset_of!(NV_ENC_LOCK_INPUT_BUFFER, reserved2), 1032, "NV_ENC_LOCK_INPUT_BUFFER.reserved2"),
        (offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, inBufferSize), 4, "NV_ENC_SEQUENCE_PARAM_PAYLOAD.inBufferSize"),
        (offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, spsppsBuffer), 16, "NV_ENC_SEQUENCE_PARAM_PAYLOAD.spsppsBuffer"),
        (offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, outSPSPPSPayloadSize), 24, "NV_ENC_SEQUENCE_PARAM_PAYLOAD.outSPSPPSPayloadSize"),
        (offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, reserved2), 1032, "NV_ENC_SEQUENCE_PARAM_PAYLOAD.reserved2"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeCaps), 64, "nvEncGetEncodeCaps"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncInitializeEncoder), 96, "nvEncInitializeEncoder"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateInputBuffer), 104, "nvEncCreateInputBuffer"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyInputBuffer), 112, "nvEncDestroyInputBuffer"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateBitstreamBuffer), 120, "nvEncCreateBitstreamBuffer"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyBitstreamBuffer), 128, "nvEncDestroyBitstreamBuffer"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncEncodePicture), 136, "nvEncEncodePicture"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLockBitstream), 144, "nvEncLockBitstream"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockBitstream), 152, "nvEncUnlockBitstream"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLockInputBuffer), 160, "nvEncLockInputBuffer"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockInputBuffer), 168, "nvEncUnlockInputBuffer"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParams), 184, "nvEncGetSequenceParams"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyEncoder), 224, "nvEncDestroyEncoder"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx), 240, "nvEncOpenEncodeSessionEx"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetLastErrorString), 304, "nvEncGetLastErrorString"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx), 320, "nvEncGetEncodePresetConfigEx"),
        (offset_of!(NV_ENCODE_API_FUNCTION_LIST, reserved2), 352, "NV_ENCODE_API_FUNCTION_LIST.reserved2"),
    ] {
        assert_eq!(offset, expected, "{name}");
    }
}

#[test]
fn constants_match_the_header() {
    for (value, expected, name) in [
        (i64::from(NVENCAPI_VERSION), 16777228, "NVENCAPI_VERSION"),
        (i64::from(NV_ENC_SUCCESS), 0, "NV_ENC_SUCCESS"),
        (i64::from(NV_ENC_ERR_UNSUPPORTED_DEVICE), 2, "NV_ENC_ERR_UNSUPPORTED_DEVICE"),
        (i64::from(NV_ENC_ERR_OUT_OF_MEMORY), 10, "NV_ENC_ERR_OUT_OF_MEMORY"),
        (i64::from(NV_ENC_ERR_LOCK_BUSY), 13, "NV_ENC_ERR_LOCK_BUSY"),
        (i64::from(NV_ENC_ERR_INVALID_VERSION), 15, "NV_ENC_ERR_INVALID_VERSION"),
        (i64::from(NV_ENC_ERR_NEED_MORE_INPUT), 17, "NV_ENC_ERR_NEED_MORE_INPUT"),
        (i64::from(NV_ENC_ERR_ENCODER_BUSY), 18, "NV_ENC_ERR_ENCODER_BUSY"),
        (i64::from(NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY), 21, "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY"),
        (i64::from(NV_ENC_DEVICE_TYPE_DIRECTX), 0, "NV_ENC_DEVICE_TYPE_DIRECTX"),
        (i64::from(NV_ENC_MEMORY_HEAP_SYSMEM_CACHED), 2, "NV_ENC_MEMORY_HEAP_SYSMEM_CACHED"),
        (i64::from(NV_ENC_BUFFER_FORMAT_NV12), 1, "NV_ENC_BUFFER_FORMAT_NV12"),
        (i64::from(NV_ENC_BUFFER_FORMAT_YUV420_10BIT), 65536, "NV_ENC_BUFFER_FORMAT_YUV420_10BIT"),
        (i64::from(NV_ENC_PARAMS_RC_VBR), 1, "NV_ENC_PARAMS_RC_VBR"),
        (i64::from(NV_ENC_PARAMS_RC_CBR), 2, "NV_ENC_PARAMS_RC_CBR"),
        (i64::from(NV_ENC_TUNING_INFO_HIGH_QUALITY), 1, "NV_ENC_TUNING_INFO_HIGH_QUALITY"),
        (i64::from(NV_ENC_PIC_STRUCT_FRAME), 1, "NV_ENC_PIC_STRUCT_FRAME"),
        (i64::from(NV_ENC_PIC_TYPE_IDR), 3, "NV_ENC_PIC_TYPE_IDR"),
        (i64::from(NV_ENC_PIC_FLAG_EOS), 8, "NV_ENC_PIC_FLAG_EOS"),
        (i64::from(NV_ENC_LEVEL_AUTOSELECT), 0, "NV_ENC_LEVEL_AUTOSELECT"),
        (i64::from(NV_ENC_TIER_HEVC_MAIN), 0, "NV_ENC_TIER_HEVC_MAIN"),
        (i64::from(NV_ENC_HEVC_CUSIZE_AUTOSELECT), 0, "NV_ENC_HEVC_CUSIZE_AUTOSELECT"),
        (i64::from(NV_ENC_H264_ENTROPY_CODING_MODE_CABAC), 1, "NV_ENC_H264_ENTROPY_CODING_MODE_CABAC"),
        (i64::from(NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED), 5, "NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED"),
        (i64::from(NV_ENC_VUI_COLOR_PRIMARIES_BT709), 1, "NV_ENC_VUI_COLOR_PRIMARIES_BT709"),
        (i64::from(NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709), 1, "NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709"),
        (i64::from(NV_ENC_VUI_MATRIX_COEFFS_BT709), 1, "NV_ENC_VUI_MATRIX_COEFFS_BT709"),
        (i64::from(NV_ENC_CAPS_NUM_MAX_BFRAMES), 0, "NV_ENC_CAPS_NUM_MAX_BFRAMES"),
        (i64::from(NV_ENC_CAPS_WIDTH_MAX), 16, "NV_ENC_CAPS_WIDTH_MAX"),
        (i64::from(NV_ENC_CAPS_HEIGHT_MAX), 17, "NV_ENC_CAPS_HEIGHT_MAX"),
        (i64::from(NV_ENC_CAPS_SUPPORT_10BIT_ENCODE), 39, "NV_ENC_CAPS_SUPPORT_10BIT_ENCODE"),
        (i64::from(NV_ENC_CAPS_WIDTH_MIN), 45, "NV_ENC_CAPS_WIDTH_MIN"),
        (i64::from(NV_ENC_CAPS_HEIGHT_MIN), 46, "NV_ENC_CAPS_HEIGHT_MIN"),
        (i64::from(NV_ENCODE_API_FUNCTION_LIST_VER), 1895956492, "NV_ENCODE_API_FUNCTION_LIST_VER"),
        (i64::from(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER), 1895890956, "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER"),
        (i64::from(NV_ENC_CAPS_PARAM_VER), 1895890956, "NV_ENC_CAPS_PARAM_VER"),
        (i64::from(NV_ENC_CONFIG_VER), 4043833356, "NV_ENC_CONFIG_VER"),
        (i64::from(NV_ENC_INITIALIZE_PARAMS_VER), 4043702284, "NV_ENC_INITIALIZE_PARAMS_VER"),
        (i64::from(NV_ENC_PRESET_CONFIG_VER), 4043571212, "NV_ENC_PRESET_CONFIG_VER"),
        (i64::from(NV_ENC_RC_PARAMS_VER), 1895890956, "NV_ENC_RC_PARAMS_VER"),
        (i64::from(NV_ENC_CREATE_INPUT_BUFFER_VER), 1895890956, "NV_ENC_CREATE_INPUT_BUFFER_VER"),
        (i64::from(NV_ENC_CREATE_BITSTREAM_BUFFER_VER), 1895890956, "NV_ENC_CREATE_BITSTREAM_BUFFER_VER"),
        (i64::from(NV_ENC_PIC_PARAMS_VER), 4043702284, "NV_ENC_PIC_PARAMS_VER"),
        (i64::from(NV_ENC_LOCK_BITSTREAM_VER), 4043374604, "NV_ENC_LOCK_BITSTREAM_VER"),
        (i64::from(NV_ENC_LOCK_INPUT_BUFFER_VER), 1895890956, "NV_ENC_LOCK_INPUT_BUFFER_VER"),
        (i64::from(NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER), 1895890956, "NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
}

#[test]
fn guids_match_the_header() {
    assert_eq!(NV_ENC_CODEC_H264_GUID.to_u128(), 0x6bc827624e634ca4aa851e50f321f6bf, "NV_ENC_CODEC_H264_GUID");
    assert_eq!(NV_ENC_H264_PROFILE_HIGH_GUID.to_u128(), 0xe7cbc3094f7a4b89af2ad537c92be310, "NV_ENC_H264_PROFILE_HIGH_GUID");
    assert_eq!(NV_ENC_PRESET_P5_GUID.to_u128(), 0x21c6e6b4297a4cba998fb6cbde72ade3, "NV_ENC_PRESET_P5_GUID");
    assert_eq!(NV_ENC_CODEC_HEVC_GUID.to_u128(), 0x790cdc8845224d7b9425bda9975f7603, "NV_ENC_CODEC_HEVC_GUID");
    assert_eq!(NV_ENC_HEVC_PROFILE_MAIN_GUID.to_u128(), 0xb514c39ab55b40fa878ff1253b4dfdec, "NV_ENC_HEVC_PROFILE_MAIN_GUID");
    assert_eq!(NV_ENC_HEVC_PROFILE_MAIN10_GUID.to_u128(), 0xfa4d2b6c3a5b411a80180a3f5e3c9be5, "NV_ENC_HEVC_PROFILE_MAIN10_GUID");
}

/// The bit-field masks of the `flags` words: the C compiler's layout of each bit-field set alone.
#[test]
fn bit_field_masks_match_the_header() {
    assert_eq!(H264_OUTPUT_AUD, 0x40, "NV_ENC_CONFIG_H264.outputAUD");
    assert_eq!(H264_REPEAT_SPSPPS, 0x1000, "NV_ENC_CONFIG_H264.repeatSPSPPS");
    assert_eq!(HEVC_OUTPUT_AUD, 0x10, "NV_ENC_CONFIG_HEVC.outputAUD");
    assert_eq!(HEVC_REPEAT_SPSPPS, 0x80, "NV_ENC_CONFIG_HEVC.repeatSPSPPS");
    assert_eq!(HEVC_CHROMA_FORMAT_MASK, 0x600, "NV_ENC_CONFIG_HEVC.chromaFormatIDC");
    assert_eq!(HEVC_PIXEL_BIT_DEPTH_MASK, 0x3800, "NV_ENC_CONFIG_HEVC.pixelBitDepthMinus8");
    assert_eq!(HEVC_CHROMA_FORMAT_MASK >> HEVC_CHROMA_FORMAT_SHIFT, 3, "chromaFormatIDC is 2 bits");
    assert_eq!(HEVC_PIXEL_BIT_DEPTH_MASK >> HEVC_PIXEL_BIT_DEPTH_SHIFT, 7, "pixelBitDepthMinus8 is 3 bits");
}

#[test]
fn a_full_session_count_is_busy_and_other_open_failures_are_not() {
    use super::session::busy;
    for s in [NV_ENC_ERR_OUT_OF_MEMORY, NV_ENC_ERR_ENCODER_BUSY, NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY] {
        assert!(busy(s), "{s}");
    }
    for s in [NV_ENC_SUCCESS, NV_ENC_ERR_UNSUPPORTED_DEVICE, NV_ENC_ERR_INVALID_VERSION, NV_ENC_ERR_LOCK_BUSY] {
        assert!(!busy(s), "{s}");
    }
}

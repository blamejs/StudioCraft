// The declarations in this file are transcribed from libva's `va/va.h`, `va/va_enc_h264.h`,
// `va/va_enc_hevc.h`, `va/va_enc_av1.h`, `va/va_drm.h` and `va/va_drmcommon.h` (libva 2.22.0),
// which carry this notice (`va_enc_av1.h` with "Copyright (c) 2021 Intel Corporation"):
//
// Copyright (c) 2007-2009 Intel Corporation. All Rights Reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sub license, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:
//
// The above copyright notice and this permission notice (including the
// next paragraph) shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NON-INFRINGEMENT.
// IN NO EVENT SHALL INTEL AND/OR ITS SUPPLIERS BE LIABLE FOR
// ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
// TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! The parts of VA-API this crate uses, as `libva.so.2` and `libva-drm.so.2` expect them: the
//! H.264, HEVC and AV1 encode parameter buffers, the packed header buffers, the rate control
//! buffers, the coded buffer segments,
//! the image structures frames are copied through, and the entry points. The sizes and field
//! offsets are checked against a C compiler's by `abi_tests.rs`.
//!
//! The structures are plain data and compile on every target, so the code that fills them in is
//! tested everywhere; only `session.rs` loads the library (64-bit Linux). The C bit-field unions
//! are stored as their `value` word, with bits allocated from the least significant end, as GCC
//! and Clang do on the little-endian targets libva runs on.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

pub type VADisplay = *mut c_void;
pub type VAStatus = c_int;
pub type VAGenericID = c_uint;
pub type VAConfigID = VAGenericID;
pub type VAContextID = VAGenericID;
pub type VASurfaceID = VAGenericID;
pub type VABufferID = VAGenericID;
pub type VAImageID = VAGenericID;
/// `VAProfile` (a C enum: `int`).
pub type VAProfile = c_int;
/// `VAEntrypoint` (a C enum: `int`).
pub type VAEntrypoint = c_int;
/// `VABufferType` (a C enum: `int`).
pub type VABufferType = c_int;
/// `VAConfigAttribType` (a C enum: `int`).
pub type VAConfigAttribType = c_int;
/// `VAEncMiscParameterType` (a C enum: `int`).
pub type VAEncMiscParameterType = c_int;
pub type VAMessageCallback = Option<unsafe extern "C" fn(user_context: *mut c_void, message: *const c_char)>;

pub const VA_STATUS_SUCCESS: VAStatus = 0;
pub const VA_STATUS_ERROR_ALLOCATION_FAILED: VAStatus = 0x02;
pub const VA_STATUS_ERROR_MAX_NUM_EXCEEDED: VAStatus = 0x0b;
pub const VA_STATUS_ERROR_UNSUPPORTED_PROFILE: VAStatus = 0x0c;
pub const VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT: VAStatus = 0x0d;
pub const VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT: VAStatus = 0x0e;
pub const VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED: VAStatus = 0x13;
pub const VA_STATUS_ERROR_UNIMPLEMENTED: VAStatus = 0x14;
pub const VA_STATUS_ERROR_HW_BUSY: VAStatus = 0x22;
pub const VA_STATUS_ERROR_TIMEDOUT: VAStatus = 0x26;

pub const VA_INVALID_ID: VAGenericID = 0xffff_ffff;
pub const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;

pub const VAProfileH264Main: VAProfile = 6;
pub const VAProfileH264High: VAProfile = 7;
pub const VAProfileH264ConstrainedBaseline: VAProfile = 13;
pub const VAProfileHEVCMain: VAProfile = 17;
pub const VAProfileAV1Profile0: VAProfile = 32;

pub const VAEntrypointEncSlice: VAEntrypoint = 6;
pub const VAEntrypointEncSliceLP: VAEntrypoint = 8;

pub const VAConfigAttribRTFormat: VAConfigAttribType = 0;
pub const VAConfigAttribRateControl: VAConfigAttribType = 5;
pub const VAConfigAttribEncPackedHeaders: VAConfigAttribType = 10;
pub const VAConfigAttribMaxPictureWidth: VAConfigAttribType = 18;
pub const VAConfigAttribMaxPictureHeight: VAConfigAttribType = 19;
pub const VAConfigAttribEncHEVCBlockSizes: VAConfigAttribType = 51;
/// `VAConfigAttribValEncAV1`: fourteen 2-bit fields, one per coding tool, each a `VA_FEATURE_*`.
pub const VAConfigAttribEncAV1: VAConfigAttribType = 52;
/// `VAConfigAttribValEncAV1Ext1`: `interpolation_filter` (bits 0 to 4, one per filter, bit 4 for
/// switchable), then the segmentation fields.
pub const VAConfigAttribEncAV1Ext1: VAConfigAttribType = 53;
/// `VAConfigAttribValEncAV1Ext2`: `tile_size_bytes_minus1` (bits 0 and 1),
/// `obu_size_bytes_minus1` (bits 2 and 3), `tx_mode_support` (bits 4 to 6: ONLY_4X4,
/// TX_MODE_LARGEST, TX_MODE_SELECT) and `max_tile_num_minus1` (bits 7 to 19).
pub const VAConfigAttribEncAV1Ext2: VAConfigAttribType = 54;
/// The value of an attribute the driver does not have.
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
/// A `VAConfigAttribEncAV1` field value: the driver needs the tool turned on.
pub const VA_FEATURE_REQUIRED: u32 = 2;

/// `VAConfigAttribEncPackedHeaders` bits: the application writes the sequence header OBU and the
/// frame header OBU.
pub const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x0000_0001;
pub const VA_ENC_PACKED_HEADER_PICTURE: u32 = 0x0000_0002;
/// `VAEncPackedHeaderParameterBuffer::type_`: `VAEncPackedHeaderSequence`, which for AV1 holds a
/// whole `sequence_header_obu()`, and `VAEncPackedHeaderPicture`, a whole `frame_header_obu()`.
pub const VAEncPackedHeaderSequence: u32 = 1;
pub const VAEncPackedHeaderPicture: u32 = 2;

pub const VA_RT_FORMAT_YUV420: c_uint = 0x0000_0001;

pub const VA_RC_CBR: u32 = 0x0000_0002;
pub const VA_RC_VBR: u32 = 0x0000_0004;
pub const VA_RC_CQP: u32 = 0x0000_0010;

/// `vaCreateContext` flag: progressive pictures only.
pub const VA_PROGRESSIVE: c_int = 0x1;

pub const VAEncCodedBufferType: VABufferType = 21;
pub const VAEncSequenceParameterBufferType: VABufferType = 22;
pub const VAEncPictureParameterBufferType: VABufferType = 23;
/// For AV1 this buffer type carries a `VAEncTileGroupBufferAV1`.
pub const VAEncSliceParameterBufferType: VABufferType = 24;
pub const VAEncPackedHeaderParameterBufferType: VABufferType = 25;
pub const VAEncPackedHeaderDataBufferType: VABufferType = 26;
pub const VAEncMiscParameterBufferType: VABufferType = 27;

pub const VAEncMiscParameterTypeFrameRate: VAEncMiscParameterType = 0;
pub const VAEncMiscParameterTypeRateControl: VAEncMiscParameterType = 1;
pub const VAEncMiscParameterTypeHRD: VAEncMiscParameterType = 5;

/// `VACodedBufferSegment::status`: the slice did not fit in the coded buffer.
pub const VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK: u32 = 0x200;
/// `VACodedBufferSegment::status`: the driver found an error in the bitstream it wrote.
pub const VA_CODED_BUF_STATUS_BAD_BITSTREAM: u32 = 0x8000;

pub const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_HEVC_INVALID: u32 = 0x0000_0001;

pub const VA_FOURCC_NV12: u32 = 0x3231_564E;
/// `drm_fourcc.h`'s NV12, the layer format of an imported NV12 DMA-BUF; the same four characters.
pub const DRM_FORMAT_NV12: u32 = 0x3231_564E;

pub type VASurfaceAttribType = c_int;
pub const VASurfaceAttribMemoryType: VASurfaceAttribType = 6;
pub const VASurfaceAttribExternalBufferDescriptor: VASurfaceAttribType = 7;
/// `VASurfaceAttrib::flags`: the attribute is set by the caller.
pub const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x0000_0002;
pub const VAGenericValueTypeInteger: c_int = 1;
pub const VAGenericValueTypePointer: c_int = 3;
/// `va_drmcommon.h`: memory described by a [`VADRMPRIMESurfaceDescriptor`].
pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;

/// The value of a [`VASurfaceAttrib`]: an `int32_t`, a `float`, a pointer or a function pointer,
/// of which this crate uses the integer and the pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub union VAGenericValueUnion {
    pub i: i32,
    pub p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAGenericValue {
    pub type_: c_int,
    pub value: VAGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASurfaceAttrib {
    pub type_: VASurfaceAttribType,
    pub flags: u32,
    pub value: VAGenericValue,
}

/// One memory object of a [`VADRMPRIMESurfaceDescriptor`]: a DMA-BUF descriptor, its size, and
/// the DRM format modifier of its layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VADRMPRIMEObject {
    pub fd: c_int,
    pub size: u32,
    pub drm_format_modifier: u64,
}

/// One layer of a [`VADRMPRIMESurfaceDescriptor`]: its DRM format and where each of its planes is.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VADRMPRIMELayer {
    pub drm_format: u32,
    pub num_planes: u32,
    pub object_index: [u32; 4],
    pub offset: [u32; 4],
    pub pitch: [u32; 4],
}

/// `va_drmcommon.h`: a surface made of DMA-BUF objects, as `vaCreateSurfaces` imports it with
/// [`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VADRMPRIMESurfaceDescriptor {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [VADRMPRIMEObject; 4],
    pub num_layers: u32,
    pub layers: [VADRMPRIMELayer; 4],
}

pub const VA_PADDING_LOW: usize = 4;
pub const VA_PADDING_MEDIUM: usize = 8;
pub const VA_PADDING_HIGH: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAConfigAttrib {
    pub type_: VAConfigAttribType,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAPictureH264 {
    /// An unused entry (`VA_INVALID_SURFACE`, `VA_PICTURE_H264_INVALID`).
    pub const INVALID: Self = Self {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VAPictureHEVC {
    pub picture_id: VASurfaceID,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAPictureHEVC {
    /// An unused entry (`VA_INVALID_SURFACE`, `VA_PICTURE_HEVC_INVALID`).
    pub const INVALID: Self = Self { picture_id: VA_INVALID_SURFACE, pic_order_cnt: 0, flags: VA_PICTURE_HEVC_INVALID, va_reserved: [0; 4] };
}

/// `VAEncSequenceParameterBufferH264`; `seq_fields` and `vui_fields` are bit-field unions.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncPictureParameterBufferH264`; `pic_fields` is a bit-field union.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VAEncPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncSliceParameterBufferH264`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncSequenceParameterBufferHEVC`; `seq_fields`, `vui_fields` and `scc_fields` are bit-field
/// unions.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VAEncSequenceParameterBufferHEVC {
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub general_tier_flag: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub seq_fields: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u32,
    pub pcm_sample_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_luma_coding_block_size_minus3: u32,
    pub log2_max_pcm_luma_coding_block_size_minus3: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub vui_num_units_in_tick: u32,
    pub vui_time_scale: u32,
    pub min_spatial_segmentation_idc: u16,
    pub max_bytes_per_pic_denom: u8,
    pub max_bits_per_min_cu_denom: u8,
    pub scc_fields: u32,
    pub va_reserved: [u32; VA_PADDING_MEDIUM - 1],
}

/// `VAEncPictureParameterBufferHEVC`; `pic_fields` and `scc_fields` are bit-field unions.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VAEncPictureParameterBufferHEVC {
    pub decoded_curr_pic: VAPictureHEVC,
    pub reference_frames: [VAPictureHEVC; 15],
    pub coded_buf: VABufferID,
    pub collocated_ref_pic_index: u8,
    pub last_picture: u8,
    pub pic_init_qp: u8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 19],
    pub row_height_minus1: [u8; 21],
    pub log2_parallel_merge_level_minus2: u8,
    pub ctu_max_bitsize_allowed: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub slice_pic_parameter_set_id: u8,
    pub nal_unit_type: u8,
    pub pic_fields: u32,
    pub hierarchical_level_plus1: u8,
    pub va_byte_reserved: u8,
    pub scc_fields: u16,
    pub va_reserved: [u32; VA_PADDING_HIGH - 1],
}

/// `VAEncSliceParameterBufferHEVC`; `slice_fields` is a bit-field union.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VAEncSliceParameterBufferHEVC {
    pub slice_segment_address: u32,
    pub num_ctu_in_slice: u32,
    pub slice_type: u8,
    pub slice_pic_parameter_set_id: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureHEVC; 15],
    pub ref_pic_list1: [VAPictureHEVC; 15],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub slice_fields: u32,
    pub pred_weight_table_bit_offset: u32,
    pub pred_weight_table_bit_length: u32,
    pub va_reserved: [u32; VA_PADDING_MEDIUM - 2],
}

/// `VAEncPackedHeaderParameterBuffer`: the type and length in bits of the header in the data
/// buffer that follows it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncSequenceParameterBufferAV1`; `seq_fields` is a bit-field union.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VAEncSequenceParameterBufferAV1 {
    pub seq_profile: u8,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub hierarchical_flag: u8,
    pub intra_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub seq_fields: u32,
    pub order_hint_bits_minus_1: u8,
    pub va_reserved: [u32; VA_PADDING_HIGH],
}

/// `VAEncSegParamAV1`; `seg_flags` is a bit-field union.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncSegParamAV1 {
    pub seg_flags: u8,
    pub segment_number: u8,
    pub feature_data: [[i16; 8]; 8],
    pub feature_mask: [u8; 8],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncWarpedMotionParamsAV1`; `wmtype` is a `VAEncTransformationTypeAV1` (a C enum: `int`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncWarpedMotionParamsAV1 {
    pub wmtype: c_int,
    pub wmmat: [i32; 8],
    pub invalid: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncPictureParameterBufferAV1`. `ref_frame_ctrl_l0` and `ref_frame_ctrl_l1`
/// (`VARefFrameCtrlAV1`), `picture_flags`, `loop_filter_flags`, `qmatrix_flags`,
/// `mode_control_flags`, `loop_restoration_flags` and `tile_group_obu_hdr_info` are bit-field
/// unions.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VAEncPictureParameterBufferAV1 {
    pub frame_width_minus_1: u16,
    pub frame_height_minus_1: u16,
    pub reconstructed_frame: VASurfaceID,
    pub coded_buf: VABufferID,
    pub reference_frames: [VASurfaceID; 8],
    pub ref_frame_idx: [u8; 7],
    pub hierarchical_level_plus1: u8,
    pub primary_ref_frame: u8,
    pub order_hint: u8,
    pub refresh_frame_flags: u8,
    pub reserved8bits1: u8,
    pub ref_frame_ctrl_l0: u32,
    pub ref_frame_ctrl_l1: u32,
    pub picture_flags: u32,
    pub seg_id_block_size: u8,
    pub num_tile_groups_minus1: u8,
    pub temporal_id: u8,
    pub filter_level: [u8; 2],
    pub filter_level_u: u8,
    pub filter_level_v: u8,
    pub loop_filter_flags: u8,
    pub superres_scale_denominator: u8,
    pub interpolation_filter: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub v_ac_delta_q: i8,
    pub min_base_qindex: u8,
    pub max_base_qindex: u8,
    pub qmatrix_flags: u16,
    pub reserved16bits1: u16,
    pub mode_control_flags: u32,
    pub segments: VAEncSegParamAV1,
    pub tile_cols: u8,
    pub tile_rows: u8,
    pub reserved16bits2: u16,
    pub width_in_sbs_minus_1: [u16; 63],
    pub height_in_sbs_minus_1: [u16; 63],
    pub context_update_tile_id: u16,
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    pub cdef_y_strengths: [u8; 8],
    pub cdef_uv_strengths: [u8; 8],
    pub loop_restoration_flags: u16,
    pub wm: [VAEncWarpedMotionParamsAV1; 7],
    pub bit_offset_qindex: u32,
    pub bit_offset_segmentation: u32,
    pub bit_offset_loopfilter_params: u32,
    pub bit_offset_cdef_params: u32,
    pub size_in_bits_cdef_params: u32,
    pub byte_offset_frame_hdr_obu_size: u32,
    pub size_in_bits_frame_hdr_obu: u32,
    pub tile_group_obu_hdr_info: u8,
    pub number_skip_frames: u8,
    pub reserved16bits3: u16,
    pub skip_frames_reduced_size: i32,
    pub va_reserved: [u32; VA_PADDING_HIGH],
}

/// `VAEncTileGroupBufferAV1`: the first and last tile of a tile group, in raster order.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncTileGroupBufferAV1 {
    pub tg_start: u8,
    pub tg_end: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncMiscParameterRateControl`; `rc_flags` is a bit-field union.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub ICQ_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncMiscParameterHRD`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncMiscParameterFrameRate`; `framerate_flags` is a bit-field union.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VAEncMiscParameterFrameRate {
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// A `VAEncMiscParameterBuffer` holding one parameter structure `T`: the C type is the `type`
/// word followed by a flexible array of `uint32_t` that holds the structure.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VAEncMiscParameterBuffer<T> {
    pub type_: VAEncMiscParameterType,
    pub data: T,
}

/// `VACodedBufferSegment`: one piece of a coded buffer, in a list ended by a null `next`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// The `libva.so.2` entry points used (all `VAStatus`-returning unless noted).
pub type vaInitialize = unsafe extern "C" fn(dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> VAStatus;
pub type vaTerminate = unsafe extern "C" fn(dpy: VADisplay) -> VAStatus;
/// Returns a static NUL-terminated string.
pub type vaErrorStr = unsafe extern "C" fn(status: VAStatus) -> *const c_char;
/// Returns a NUL-terminated string owned by the driver.
pub type vaQueryVendorString = unsafe extern "C" fn(dpy: VADisplay) -> *const c_char;
pub type vaSetInfoCallback = unsafe extern "C" fn(dpy: VADisplay, callback: VAMessageCallback, user_context: *mut c_void) -> VAMessageCallback;
pub type vaSetErrorCallback = unsafe extern "C" fn(dpy: VADisplay, callback: VAMessageCallback, user_context: *mut c_void) -> VAMessageCallback;
/// Returns the most entry points `vaQueryConfigEntrypoints` writes.
pub type vaMaxNumEntrypoints = unsafe extern "C" fn(dpy: VADisplay) -> c_int;
pub type vaQueryConfigEntrypoints =
    unsafe extern "C" fn(dpy: VADisplay, profile: VAProfile, entrypoint_list: *mut VAEntrypoint, num_entrypoints: *mut c_int) -> VAStatus;
pub type vaGetConfigAttributes = unsafe extern "C" fn(
    dpy: VADisplay,
    profile: VAProfile,
    entrypoint: VAEntrypoint,
    attrib_list: *mut VAConfigAttrib,
    num_attribs: c_int,
) -> VAStatus;
pub type vaCreateConfig = unsafe extern "C" fn(
    dpy: VADisplay,
    profile: VAProfile,
    entrypoint: VAEntrypoint,
    attrib_list: *mut VAConfigAttrib,
    num_attribs: c_int,
    config_id: *mut VAConfigID,
) -> VAStatus;
pub type vaDestroyConfig = unsafe extern "C" fn(dpy: VADisplay, config_id: VAConfigID) -> VAStatus;
/// The last two arguments are `VASurfaceAttrib *attrib_list, unsigned int num_attribs`: none for
/// the session's own surfaces, and a [`VASurfaceAttrib`] list naming a DMA-BUF for an imported one.
pub type vaCreateSurfaces = unsafe extern "C" fn(
    dpy: VADisplay,
    format: c_uint,
    width: c_uint,
    height: c_uint,
    surfaces: *mut VASurfaceID,
    num_surfaces: c_uint,
    attrib_list: *mut c_void,
    num_attribs: c_uint,
) -> VAStatus;
pub type vaDestroySurfaces = unsafe extern "C" fn(dpy: VADisplay, surfaces: *mut VASurfaceID, num_surfaces: c_int) -> VAStatus;
pub type vaCreateContext = unsafe extern "C" fn(
    dpy: VADisplay,
    config_id: VAConfigID,
    picture_width: c_int,
    picture_height: c_int,
    flag: c_int,
    render_targets: *mut VASurfaceID,
    num_render_targets: c_int,
    context: *mut VAContextID,
) -> VAStatus;
pub type vaDestroyContext = unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus;
pub type vaCreateBuffer = unsafe extern "C" fn(
    dpy: VADisplay,
    context: VAContextID,
    type_: VABufferType,
    size: c_uint,
    num_elements: c_uint,
    data: *mut c_void,
    buf_id: *mut VABufferID,
) -> VAStatus;
pub type vaDestroyBuffer = unsafe extern "C" fn(dpy: VADisplay, buffer_id: VABufferID) -> VAStatus;
pub type vaBeginPicture = unsafe extern "C" fn(dpy: VADisplay, context: VAContextID, render_target: VASurfaceID) -> VAStatus;
pub type vaRenderPicture = unsafe extern "C" fn(dpy: VADisplay, context: VAContextID, buffers: *mut VABufferID, num_buffers: c_int) -> VAStatus;
pub type vaEndPicture = unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus;
pub type vaSyncSurface = unsafe extern "C" fn(dpy: VADisplay, render_target: VASurfaceID) -> VAStatus;
/// libva 2.9 and later: [`vaSyncSurface`] that returns `VA_STATUS_ERROR_TIMEDOUT` after
/// `timeout_ns`, or `VA_STATUS_ERROR_UNIMPLEMENTED` from a driver without it.
pub type vaSyncSurface2 = unsafe extern "C" fn(dpy: VADisplay, surface: VASurfaceID, timeout_ns: u64) -> VAStatus;
pub type vaCreateImage =
    unsafe extern "C" fn(dpy: VADisplay, format: *mut VAImageFormat, width: c_int, height: c_int, image: *mut VAImage) -> VAStatus;
pub type vaDeriveImage = unsafe extern "C" fn(dpy: VADisplay, surface: VASurfaceID, image: *mut VAImage) -> VAStatus;
pub type vaDestroyImage = unsafe extern "C" fn(dpy: VADisplay, image: VAImageID) -> VAStatus;
pub type vaPutImage = unsafe extern "C" fn(
    dpy: VADisplay,
    surface: VASurfaceID,
    image: VAImageID,
    src_x: c_int,
    src_y: c_int,
    src_width: c_uint,
    src_height: c_uint,
    dest_x: c_int,
    dest_y: c_int,
    dest_width: c_uint,
    dest_height: c_uint,
) -> VAStatus;
pub type vaMapBuffer = unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID, pbuf: *mut *mut c_void) -> VAStatus;
pub type vaUnmapBuffer = unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID) -> VAStatus;
/// `libva-drm.so.2`: the display of a DRM device (render node) file descriptor.
pub type vaGetDisplayDRM = unsafe extern "C" fn(fd: c_int) -> VADisplay;

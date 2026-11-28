//! The sizes and field offsets of every structure `abi.rs` declares, as GCC lays them out from
//! libva 2.22.0's headers on x86-64 Linux (`sizeof` and `offsetof` of each). The structures hold
//! no pointers except `VACodedBufferSegment`, whose numbers are for 64-bit targets, where the
//! session code runs. The AV1 and packed header numbers were taken with MSVC on x86-64 from the
//! same headers. Those structures hold no pointers, and each of their bit-field unions is one
//! value of the union's field type, which both compilers lay out alike.

use std::mem::{offset_of, size_of};

use super::abi::*;

#[test]
fn image_structures_match_va_h() {
    assert_eq!(size_of::<VAPictureH264>(), 36);
    assert_eq!(size_of::<VAPictureHEVC>(), 28);
    assert_eq!(size_of::<VAImageFormat>(), 48);
    assert_eq!(size_of::<VAImage>(), 120);
    assert_eq!(offset_of!(VAImage, buf), 52);
    assert_eq!(offset_of!(VAImage, width), 56);
    assert_eq!(offset_of!(VAImage, num_planes), 64);
    assert_eq!(offset_of!(VAImage, pitches), 68);
    assert_eq!(offset_of!(VAImage, offsets), 80);
    assert_eq!(size_of::<VAConfigAttrib>(), 8);
}

#[test]
fn h264_sequence_parameters_match_va_enc_h264_h() {
    type T = VAEncSequenceParameterBufferH264;
    assert_eq!(size_of::<T>(), 1132);
    assert_eq!(offset_of!(T, level_idc), 1);
    assert_eq!(offset_of!(T, intra_period), 4);
    assert_eq!(offset_of!(T, bits_per_second), 16);
    assert_eq!(offset_of!(T, max_num_ref_frames), 20);
    assert_eq!(offset_of!(T, picture_width_in_mbs), 24);
    assert_eq!(offset_of!(T, picture_height_in_mbs), 26);
    assert_eq!(offset_of!(T, seq_fields), 28);
    assert_eq!(offset_of!(T, bit_depth_luma_minus8), 32);
    assert_eq!(offset_of!(T, num_ref_frames_in_pic_order_cnt_cycle), 34);
    assert_eq!(offset_of!(T, offset_for_non_ref_pic), 36);
    assert_eq!(offset_of!(T, offset_for_ref_frame), 44);
    assert_eq!(offset_of!(T, frame_cropping_flag), 1068);
    assert_eq!(offset_of!(T, frame_crop_left_offset), 1072);
    assert_eq!(offset_of!(T, frame_crop_bottom_offset), 1084);
    assert_eq!(offset_of!(T, vui_parameters_present_flag), 1088);
    assert_eq!(offset_of!(T, vui_fields), 1092);
    assert_eq!(offset_of!(T, aspect_ratio_idc), 1096);
    assert_eq!(offset_of!(T, sar_width), 1100);
    assert_eq!(offset_of!(T, time_scale), 1112);
    assert_eq!(offset_of!(T, va_reserved), 1116);
}

#[test]
fn h264_picture_parameters_match_va_enc_h264_h() {
    type T = VAEncPictureParameterBufferH264;
    assert_eq!(size_of::<T>(), 648);
    assert_eq!(offset_of!(T, ReferenceFrames), 36);
    assert_eq!(offset_of!(T, coded_buf), 612);
    assert_eq!(offset_of!(T, pic_parameter_set_id), 616);
    assert_eq!(offset_of!(T, last_picture), 618);
    assert_eq!(offset_of!(T, frame_num), 620);
    assert_eq!(offset_of!(T, pic_init_qp), 622);
    assert_eq!(offset_of!(T, second_chroma_qp_index_offset), 626);
    assert_eq!(offset_of!(T, pic_fields), 628);
    assert_eq!(offset_of!(T, va_reserved), 632);
}

#[test]
fn h264_slice_parameters_match_va_enc_h264_h() {
    type T = VAEncSliceParameterBufferH264;
    assert_eq!(size_of::<T>(), 3140);
    assert_eq!(offset_of!(T, macroblock_info), 8);
    assert_eq!(offset_of!(T, slice_type), 12);
    assert_eq!(offset_of!(T, idr_pic_id), 14);
    assert_eq!(offset_of!(T, pic_order_cnt_lsb), 16);
    assert_eq!(offset_of!(T, delta_pic_order_cnt_bottom), 20);
    assert_eq!(offset_of!(T, delta_pic_order_cnt), 24);
    assert_eq!(offset_of!(T, direct_spatial_mv_pred_flag), 32);
    assert_eq!(offset_of!(T, num_ref_idx_l1_active_minus1), 35);
    assert_eq!(offset_of!(T, RefPicList0), 36);
    assert_eq!(offset_of!(T, RefPicList1), 1188);
    assert_eq!(offset_of!(T, luma_log2_weight_denom), 2340);
    assert_eq!(offset_of!(T, luma_weight_l0_flag), 2342);
    assert_eq!(offset_of!(T, luma_weight_l0), 2344);
    assert_eq!(offset_of!(T, luma_offset_l0), 2408);
    assert_eq!(offset_of!(T, chroma_weight_l0_flag), 2472);
    assert_eq!(offset_of!(T, chroma_weight_l0), 2474);
    assert_eq!(offset_of!(T, chroma_offset_l0), 2602);
    assert_eq!(offset_of!(T, luma_weight_l1_flag), 2730);
    assert_eq!(offset_of!(T, luma_weight_l1), 2732);
    assert_eq!(offset_of!(T, chroma_weight_l1_flag), 2860);
    assert_eq!(offset_of!(T, chroma_offset_l1), 2990);
    assert_eq!(offset_of!(T, cabac_init_idc), 3118);
    assert_eq!(offset_of!(T, slice_beta_offset_div2), 3122);
    assert_eq!(offset_of!(T, va_reserved), 3124);
}

#[test]
fn hevc_sequence_parameters_match_va_enc_hevc_h() {
    type T = VAEncSequenceParameterBufferHEVC;
    assert_eq!(size_of::<T>(), 116);
    assert_eq!(offset_of!(T, general_tier_flag), 2);
    assert_eq!(offset_of!(T, intra_period), 4);
    assert_eq!(offset_of!(T, bits_per_second), 16);
    assert_eq!(offset_of!(T, pic_width_in_luma_samples), 20);
    assert_eq!(offset_of!(T, seq_fields), 24);
    assert_eq!(offset_of!(T, log2_min_luma_coding_block_size_minus3), 28);
    assert_eq!(offset_of!(T, max_transform_hierarchy_depth_intra), 33);
    assert_eq!(offset_of!(T, pcm_sample_bit_depth_luma_minus1), 36);
    assert_eq!(offset_of!(T, log2_max_pcm_luma_coding_block_size_minus3), 48);
    assert_eq!(offset_of!(T, vui_parameters_present_flag), 52);
    assert_eq!(offset_of!(T, vui_fields), 56);
    assert_eq!(offset_of!(T, aspect_ratio_idc), 60);
    assert_eq!(offset_of!(T, sar_width), 64);
    assert_eq!(offset_of!(T, vui_time_scale), 76);
    assert_eq!(offset_of!(T, min_spatial_segmentation_idc), 80);
    assert_eq!(offset_of!(T, max_bits_per_min_cu_denom), 83);
    assert_eq!(offset_of!(T, scc_fields), 84);
    assert_eq!(offset_of!(T, va_reserved), 88);
}

#[test]
fn hevc_picture_parameters_match_va_enc_hevc_h() {
    type T = VAEncPictureParameterBufferHEVC;
    assert_eq!(size_of::<T>(), 576);
    assert_eq!(offset_of!(T, reference_frames), 28);
    assert_eq!(offset_of!(T, coded_buf), 448);
    assert_eq!(offset_of!(T, collocated_ref_pic_index), 452);
    assert_eq!(offset_of!(T, pps_cr_qp_offset), 457);
    assert_eq!(offset_of!(T, column_width_minus1), 460);
    assert_eq!(offset_of!(T, row_height_minus1), 479);
    assert_eq!(offset_of!(T, log2_parallel_merge_level_minus2), 500);
    assert_eq!(offset_of!(T, nal_unit_type), 505);
    assert_eq!(offset_of!(T, pic_fields), 508);
    assert_eq!(offset_of!(T, hierarchical_level_plus1), 512);
    assert_eq!(offset_of!(T, scc_fields), 514);
    assert_eq!(offset_of!(T, va_reserved), 516);
}

#[test]
fn hevc_slice_parameters_match_va_enc_hevc_h() {
    type T = VAEncSliceParameterBufferHEVC;
    assert_eq!(size_of::<T>(), 1076);
    assert_eq!(offset_of!(T, slice_type), 8);
    assert_eq!(offset_of!(T, num_ref_idx_l1_active_minus1), 11);
    assert_eq!(offset_of!(T, ref_pic_list0), 12);
    assert_eq!(offset_of!(T, ref_pic_list1), 432);
    assert_eq!(offset_of!(T, luma_log2_weight_denom), 852);
    assert_eq!(offset_of!(T, delta_luma_weight_l0), 854);
    assert_eq!(offset_of!(T, delta_chroma_weight_l0), 884);
    assert_eq!(offset_of!(T, chroma_offset_l1), 1004);
    assert_eq!(offset_of!(T, max_num_merge_cand), 1034);
    assert_eq!(offset_of!(T, slice_tc_offset_div2), 1039);
    assert_eq!(offset_of!(T, slice_fields), 1040);
    assert_eq!(offset_of!(T, pred_weight_table_bit_offset), 1044);
    assert_eq!(offset_of!(T, va_reserved), 1052);
}

#[test]
fn packed_header_parameters_match_va_h() {
    type T = VAEncPackedHeaderParameterBuffer;
    assert_eq!(size_of::<T>(), 28);
    assert_eq!(offset_of!(T, bit_length), 4);
    assert_eq!(offset_of!(T, has_emulation_bytes), 8);
    assert_eq!(offset_of!(T, va_reserved), 12);
}

#[test]
fn av1_sequence_parameters_match_va_enc_av1_h() {
    type T = VAEncSequenceParameterBufferAV1;
    assert_eq!(size_of::<T>(), 88);
    assert_eq!(offset_of!(T, seq_level_idx), 1);
    assert_eq!(offset_of!(T, seq_tier), 2);
    assert_eq!(offset_of!(T, hierarchical_flag), 3);
    assert_eq!(offset_of!(T, intra_period), 4);
    assert_eq!(offset_of!(T, ip_period), 8);
    assert_eq!(offset_of!(T, bits_per_second), 12);
    assert_eq!(offset_of!(T, seq_fields), 16);
    assert_eq!(offset_of!(T, order_hint_bits_minus_1), 20);
    assert_eq!(offset_of!(T, va_reserved), 24);
}

#[test]
fn av1_picture_parameters_match_va_enc_av1_h() {
    assert_eq!(size_of::<VAEncSegParamAV1>(), 156);
    assert_eq!(offset_of!(VAEncSegParamAV1, segment_number), 1);
    assert_eq!(offset_of!(VAEncSegParamAV1, feature_data), 2);
    assert_eq!(offset_of!(VAEncSegParamAV1, feature_mask), 130);
    assert_eq!(offset_of!(VAEncSegParamAV1, va_reserved), 140);
    assert_eq!(size_of::<VAEncWarpedMotionParamsAV1>(), 56);
    assert_eq!(offset_of!(VAEncWarpedMotionParamsAV1, wmmat), 4);
    assert_eq!(offset_of!(VAEncWarpedMotionParamsAV1, invalid), 36);
    assert_eq!(offset_of!(VAEncWarpedMotionParamsAV1, va_reserved), 40);

    type T = VAEncPictureParameterBufferAV1;
    assert_eq!(size_of::<T>(), 1032);
    let offsets = [
        (offset_of!(T, frame_height_minus_1), 2),
        (offset_of!(T, reconstructed_frame), 4),
        (offset_of!(T, coded_buf), 8),
        (offset_of!(T, reference_frames), 12),
        (offset_of!(T, ref_frame_idx), 44),
        (offset_of!(T, hierarchical_level_plus1), 51),
        (offset_of!(T, primary_ref_frame), 52),
        (offset_of!(T, order_hint), 53),
        (offset_of!(T, refresh_frame_flags), 54),
        (offset_of!(T, reserved8bits1), 55),
        (offset_of!(T, ref_frame_ctrl_l0), 56),
        (offset_of!(T, ref_frame_ctrl_l1), 60),
        (offset_of!(T, picture_flags), 64),
        (offset_of!(T, seg_id_block_size), 68),
        (offset_of!(T, num_tile_groups_minus1), 69),
        (offset_of!(T, temporal_id), 70),
        (offset_of!(T, filter_level), 71),
        (offset_of!(T, filter_level_u), 73),
        (offset_of!(T, filter_level_v), 74),
        (offset_of!(T, loop_filter_flags), 75),
        (offset_of!(T, superres_scale_denominator), 76),
        (offset_of!(T, interpolation_filter), 77),
        (offset_of!(T, ref_deltas), 78),
        (offset_of!(T, mode_deltas), 86),
        (offset_of!(T, base_qindex), 88),
        (offset_of!(T, y_dc_delta_q), 89),
        (offset_of!(T, v_ac_delta_q), 93),
        (offset_of!(T, min_base_qindex), 94),
        (offset_of!(T, max_base_qindex), 95),
        (offset_of!(T, qmatrix_flags), 96),
        (offset_of!(T, reserved16bits1), 98),
        (offset_of!(T, mode_control_flags), 100),
        (offset_of!(T, segments), 104),
        (offset_of!(T, tile_cols), 260),
        (offset_of!(T, tile_rows), 261),
        (offset_of!(T, reserved16bits2), 262),
        (offset_of!(T, width_in_sbs_minus_1), 264),
        (offset_of!(T, height_in_sbs_minus_1), 390),
        (offset_of!(T, context_update_tile_id), 516),
        (offset_of!(T, cdef_damping_minus_3), 518),
        (offset_of!(T, cdef_bits), 519),
        (offset_of!(T, cdef_y_strengths), 520),
        (offset_of!(T, cdef_uv_strengths), 528),
        (offset_of!(T, loop_restoration_flags), 536),
        (offset_of!(T, wm), 540),
        (offset_of!(T, bit_offset_qindex), 932),
        (offset_of!(T, bit_offset_segmentation), 936),
        (offset_of!(T, bit_offset_loopfilter_params), 940),
        (offset_of!(T, bit_offset_cdef_params), 944),
        (offset_of!(T, size_in_bits_cdef_params), 948),
        (offset_of!(T, byte_offset_frame_hdr_obu_size), 952),
        (offset_of!(T, size_in_bits_frame_hdr_obu), 956),
        (offset_of!(T, tile_group_obu_hdr_info), 960),
        (offset_of!(T, number_skip_frames), 961),
        (offset_of!(T, reserved16bits3), 962),
        (offset_of!(T, skip_frames_reduced_size), 964),
        (offset_of!(T, va_reserved), 968),
    ];
    for (i, (got, want)) in offsets.into_iter().enumerate() {
        assert_eq!(got, want, "field {i} of the list");
    }

    assert_eq!(size_of::<VAEncTileGroupBufferAV1>(), 20);
    assert_eq!(offset_of!(VAEncTileGroupBufferAV1, tg_end), 1);
    assert_eq!(offset_of!(VAEncTileGroupBufferAV1, va_reserved), 4);
}

#[test]
fn rate_control_and_coded_buffers_match_va_h() {
    assert_eq!(size_of::<VAEncMiscParameterRateControl>(), 60);
    assert_eq!(offset_of!(VAEncMiscParameterRateControl, rc_flags), 24);
    assert_eq!(offset_of!(VAEncMiscParameterRateControl, ICQ_quality_factor), 28);
    assert_eq!(offset_of!(VAEncMiscParameterRateControl, target_frame_size), 40);
    assert_eq!(size_of::<VAEncMiscParameterHRD>(), 24);
    assert_eq!(size_of::<VAEncMiscParameterFrameRate>(), 24);
    // The C header is the 4-byte type word and a flexible array; the parameters follow at 4.
    assert_eq!(offset_of!(VAEncMiscParameterBuffer<VAEncMiscParameterHRD>, data), 4);
    assert_eq!(size_of::<VAEncMiscParameterBuffer<VAEncMiscParameterRateControl>>(), 4 + 60);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<VACodedBufferSegment>(), 48);
        assert_eq!(offset_of!(VACodedBufferSegment, buf), 16);
        assert_eq!(offset_of!(VACodedBufferSegment, next), 24);
    }
}

#[test]
fn surface_import_structures_match_va_h_and_va_drmcommon_h() {
    assert_eq!(size_of::<VADRMPRIMEObject>(), 16);
    assert_eq!(offset_of!(VADRMPRIMEObject, drm_format_modifier), 8);
    assert_eq!(size_of::<VADRMPRIMELayer>(), 56);
    assert_eq!(offset_of!(VADRMPRIMELayer, offset), 24);
    assert_eq!(offset_of!(VADRMPRIMELayer, pitch), 40);
    type T = VADRMPRIMESurfaceDescriptor;
    assert_eq!(size_of::<T>(), 312);
    assert_eq!(offset_of!(T, objects), 16);
    assert_eq!(offset_of!(T, num_layers), 80);
    assert_eq!(offset_of!(T, layers), 84);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<VAGenericValue>(), 16);
        assert_eq!(offset_of!(VAGenericValue, value), 8);
        assert_eq!(size_of::<VASurfaceAttrib>(), 24);
        assert_eq!(offset_of!(VASurfaceAttrib, value), 8);
    }
}

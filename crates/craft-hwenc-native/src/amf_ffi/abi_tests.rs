//! The layouts `abi.rs` declares against AMF 1.5.3's headers: the sizes and field offsets of the
//! plain structures as MSVC lays them out on x86-64, and the slot of every table entry this crate
//! calls, counted in the header's order (each slot is one pointer).

use std::mem::{align_of, offset_of, size_of};

use super::abi::*;

/// The slot of the table entry at byte `offset`.
fn slot(offset: usize) -> usize {
    assert_eq!(offset % size_of::<usize>(), 0, "a table entry off a pointer boundary");
    offset / size_of::<usize>()
}

/// The number of entries of the table `T`.
fn entries<T>() -> usize {
    size_of::<T>() / size_of::<usize>()
}

#[test]
fn plain_structures_match_platform_h_and_variant_h() {
    assert_eq!(size_of::<AMFGuid>(), 16);
    assert_eq!(offset_of!(AMFGuid, data4), 8);
    assert_eq!((size_of::<AMFSize>(), size_of::<AMFRate>()), (8, 8));
    assert_eq!(size_of::<AMFVariantValue>(), 16);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<AMFVariantStruct>(), 24);
        assert_eq!(align_of::<AMFVariantStruct>(), 8);
        assert_eq!(offset_of!(AMFVariantStruct, value), 8);
    }
}

#[test]
fn constants_match_the_headers() {
    assert_eq!(AMF_FULL_VERSION, 0x0001_0005_0003_0000);
    assert_eq!(make_version(1, 4, 0, 0), 0x0001_0004_0000_0000);
    assert_eq!(IID_AMF_BUFFER.data1, 0xb04b7248);
    assert_eq!(IID_AMF_BUFFER.data4, [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb]);
    // The positions in `AMF_RESULT`, which counts up from AMF_OK = 0 without gaps.
    let results = [
        (AMF_INVALID_ARG, 4),
        (AMF_OUT_OF_RANGE, 5),
        (AMF_NOT_SUPPORTED, 10),
        (AMF_NOT_FOUND, 11),
        (AMF_NO_DEVICE, 17),
        (AMF_EOF, 23),
        (AMF_REPEAT, 24),
        (AMF_INPUT_FULL, 25),
        (AMF_CODEC_NOT_SUPPORTED, 30),
        (AMF_ENCODER_NOT_PRESENT, 36),
        (AMF_NEED_MORE_INPUT, 44),
    ];
    for (got, want) in results {
        assert_eq!(got, want);
    }
}

#[test]
fn property_values_carry_their_variant_type() {
    let v = AMFVariantStruct::int64(-7);
    assert_eq!(v.type_, AMF_VARIANT_INT64);
    // SAFETY: the constructor wrote the 64-bit integer member.
    assert_eq!(unsafe { v.value.int64Value }, -7);
    let s = AMFVariantStruct::size(1920, 1080);
    assert_eq!(s.type_, AMF_VARIANT_SIZE);
    // SAFETY: the constructor wrote the size member.
    assert_eq!(unsafe { s.value.sizeValue }, AMFSize { width: 1920, height: 1080 });
    let r = AMFVariantStruct::rate(30000, 1001);
    assert_eq!(r.type_, AMF_VARIANT_RATE);
    // SAFETY: the constructor wrote the rate member.
    assert_eq!(unsafe { r.value.rateValue }, AMFRate { num: 30000, den: 1001 });
    let b = AMFVariantStruct::bool(true);
    assert_eq!(b.type_, AMF_VARIANT_BOOL);
    // SAFETY: every member starts at the union's first byte, and the constructor cleared the rest.
    assert_eq!(unsafe { b.value.int64Value }, 1, "the other bytes of the union are zero");
}

#[test]
fn property_storage_and_data_tables_match_property_storage_h_and_data_h() {
    type S = PropertyStorageVtbl<AMFData>;
    assert_eq!(entries::<S>(), 13);
    assert_eq!(slot(offset_of!(S, Release)), 1);
    assert_eq!(slot(offset_of!(S, QueryInterface)), 2);
    assert_eq!(slot(offset_of!(S, SetProperty)), 3);
    assert_eq!(slot(offset_of!(S, GetProperty)), 4);
    assert_eq!(entries::<AMFInterfaceVtbl>(), 3);
    assert_eq!(entries::<AMFDataVtbl>(), 23);
    assert_eq!(slot(offset_of!(AMFDataVtbl, data) + offset_of!(DataVtbl<AMFData>, SetPts)), 19);
    assert_eq!(slot(offset_of!(AMFDataVtbl, data) + offset_of!(DataVtbl<AMFData>, GetPts)), 20);
    assert_eq!(slot(offset_of!(AMFDataVtbl, data) + offset_of!(DataVtbl<AMFData>, SetDuration)), 21);
}

#[test]
fn surface_plane_and_buffer_tables_match_surface_h_plane_h_and_buffer_h() {
    assert_eq!(entries::<AMFSurfaceVtbl>(), 33);
    assert_eq!(slot(offset_of!(AMFSurfaceVtbl, GetPlane)), 26);
    assert_eq!(entries::<AMFPlaneVtbl>(), 13);
    assert_eq!(slot(offset_of!(AMFPlaneVtbl, GetNative)), 4);
    assert_eq!(slot(offset_of!(AMFPlaneVtbl, GetHeight)), 9);
    assert_eq!(slot(offset_of!(AMFPlaneVtbl, GetHPitch)), 10);
    assert_eq!(entries::<AMFBufferVtbl>(), 28);
    assert_eq!(slot(offset_of!(AMFBufferVtbl, GetSize)), 24);
    assert_eq!(slot(offset_of!(AMFBufferVtbl, GetNative)), 25);
}

#[test]
fn context_component_and_factory_tables_match_context_h_component_h_and_factory_h() {
    assert_eq!(entries::<AMFContextVtbl>(), 55);
    assert_eq!(slot(offset_of!(AMFContextVtbl, Terminate)), 13);
    assert_eq!(slot(offset_of!(AMFContextVtbl, InitDX11)), 18);
    assert_eq!(slot(offset_of!(AMFContextVtbl, AllocSurface)), 44);
    assert_eq!(entries::<AMFComponentVtbl>(), 28);
    assert_eq!(slot(offset_of!(AMFComponentVtbl, Init)), 17);
    assert_eq!(slot(offset_of!(AMFComponentVtbl, Terminate)), 19);
    assert_eq!(slot(offset_of!(AMFComponentVtbl, Drain)), 20);
    assert_eq!(slot(offset_of!(AMFComponentVtbl, SubmitInput)), 22);
    assert_eq!(slot(offset_of!(AMFComponentVtbl, QueryOutput)), 23);
    assert_eq!(entries::<AMFFactoryVtbl>(), 7);
    assert_eq!(slot(offset_of!(AMFFactoryVtbl, CreateComponent)), 1);
}

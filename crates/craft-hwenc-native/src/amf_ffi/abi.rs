// The declarations in this file are transcribed from AMD's Advanced Media Framework SDK headers
// `core/Platform.h`, `core/Result.h`, `core/Variant.h`, `core/Interface.h`, `core/Data.h`,
// `core/Plane.h`, `core/Surface.h`, `core/Buffer.h`, `core/Context.h`, `core/Factory.h`,
// `core/Version.h` and `components/Component.h` (AMF 1.5.3,
// https://github.com/GPUOpen-LibrariesAndSDKs/AMF), which carry this notice (with copyright
// years from 2017 to 2026):
//
// MIT license
//
// Copyright (c) 2018 Advanced Micro Devices, Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! The parts of AMF this crate uses, as `amfrt64.dll` expects them on 64-bit Windows: the entry
//! points, the variant that carries property values, and the interface tables of the factory, the
//! context, the encoder component, surfaces, planes and buffers. Each table lists every entry in
//! the header's order; the entries this crate never calls are pointer-sized placeholders.
//! `abi_tests.rs` checks the sizes, field offsets and table slots.
//!
//! The declarations are plain data and compile on every target, so the code that builds property
//! values is tested everywhere; only `session.rs` loads the library (Windows). Strings are
//! Windows' 16-bit `wchar_t`, and `amf_long` is Windows' 32-bit `long`.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::{c_int, c_void};

pub type AMF_RESULT = c_int;
/// `amf_long`: `long`, 32 bits on Windows.
pub type amf_long = i32;
/// `amf_bool` in the C interface: `amf_uint8`.
pub type amf_bool = u8;
/// `amf_pts`: a time in 100 ns units.
pub type amf_pts = i64;
/// `wchar_t` on Windows.
pub type wchar = u16;
/// `AMF_MEMORY_TYPE`, `AMF_SURFACE_FORMAT`, `AMF_DX_VERSION`, `AMF_PLANE_TYPE` and
/// `AMF_VARIANT_TYPE` are C enums: `int`.
pub type AMF_ENUM = c_int;

pub const AMF_OK: AMF_RESULT = 0;
pub const AMF_FAIL: AMF_RESULT = 1;
pub const AMF_INVALID_ARG: AMF_RESULT = 4;
pub const AMF_OUT_OF_RANGE: AMF_RESULT = 5;
pub const AMF_OUT_OF_MEMORY: AMF_RESULT = 6;
pub const AMF_NOT_SUPPORTED: AMF_RESULT = 10;
pub const AMF_NOT_FOUND: AMF_RESULT = 11;
pub const AMF_NO_DEVICE: AMF_RESULT = 17;
pub const AMF_DIRECTX_FAILED: AMF_RESULT = 18;
pub const AMF_EOF: AMF_RESULT = 23;
pub const AMF_REPEAT: AMF_RESULT = 24;
pub const AMF_INPUT_FULL: AMF_RESULT = 25;
pub const AMF_INVALID_RESOLUTION: AMF_RESULT = 29;
pub const AMF_CODEC_NOT_SUPPORTED: AMF_RESULT = 30;
pub const AMF_SURFACE_FORMAT_NOT_SUPPORTED: AMF_RESULT = 31;
pub const AMF_ENCODER_NOT_PRESENT: AMF_RESULT = 36;
pub const AMF_NEED_MORE_INPUT: AMF_RESULT = 44;

pub const AMF_MEMORY_HOST: AMF_ENUM = 1;
pub const AMF_SURFACE_NV12: AMF_ENUM = 1;
pub const AMF_DX11_0: AMF_ENUM = 110;
pub const AMF_PLANE_Y: AMF_ENUM = 2;
pub const AMF_PLANE_UV: AMF_ENUM = 3;

pub const AMF_VARIANT_EMPTY: AMF_ENUM = 0;
pub const AMF_VARIANT_BOOL: AMF_ENUM = 1;
pub const AMF_VARIANT_INT64: AMF_ENUM = 2;
pub const AMF_VARIANT_SIZE: AMF_ENUM = 5;
pub const AMF_VARIANT_RATE: AMF_ENUM = 7;
pub const AMF_VARIANT_INTERFACE: AMF_ENUM = 12;

/// `AMF_FULL_VERSION` of the headers this file is transcribed from: 1.5.3.0, as
/// `AMF_MAKE_FULL_VERSION` packs it.
pub const AMF_FULL_VERSION: u64 = make_version(1, 5, 3, 0);

/// `AMF_MAKE_FULL_VERSION`.
pub const fn make_version(major: u16, minor: u16, release: u16, build: u16) -> u64 {
    ((major as u64) << 48) | ((minor as u64) << 32) | ((release as u64) << 16) | build as u64
}

/// `AMFInit_Fn` (`AMF_CDECL_CALL`).
pub type AMFInit_Fn = unsafe extern "C" fn(version: u64, factory: *mut *mut AMFFactory) -> AMF_RESULT;
/// `AMFQueryVersion_Fn` (`AMF_CDECL_CALL`).
pub type AMFQueryVersion_Fn = unsafe extern "C" fn(version: *mut u64) -> AMF_RESULT;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AMFGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// `IID_AMFBuffer` (`core/Buffer.h`).
pub const IID_AMF_BUFFER: AMFGuid =
    AMFGuid { data1: 0xb04b7248, data2: 0xb6f0, data3: 0x4321, data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb] };

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AMFSize {
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AMFRate {
    pub num: u32,
    pub den: u32,
}

/// The value union of `AMFVariantStruct`. `rect` stands for its largest members (`AMFRect`,
/// `AMFFloatVector4D`), so the union has the header's size.
#[repr(C)]
#[derive(Clone, Copy)]
pub union AMFVariantValue {
    pub boolValue: amf_bool,
    pub int64Value: i64,
    pub doubleValue: f64,
    pub pInterface: *mut AMFInterface,
    pub sizeValue: AMFSize,
    pub rateValue: AMFRate,
    pub rect: [i32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AMFVariantStruct {
    pub type_: AMF_ENUM,
    pub value: AMFVariantValue,
}

impl AMFVariantStruct {
    pub fn empty() -> AMFVariantStruct {
        AMFVariantStruct { type_: AMF_VARIANT_EMPTY, value: AMFVariantValue { rect: [0; 4] } }
    }

    pub fn int64(v: i64) -> AMFVariantStruct {
        let mut s = AMFVariantStruct::empty();
        s.type_ = AMF_VARIANT_INT64;
        s.value.int64Value = v;
        s
    }

    pub fn bool(v: bool) -> AMFVariantStruct {
        let mut s = AMFVariantStruct::empty();
        s.type_ = AMF_VARIANT_BOOL;
        s.value.boolValue = u8::from(v);
        s
    }

    pub fn size(width: i32, height: i32) -> AMFVariantStruct {
        let mut s = AMFVariantStruct::empty();
        s.type_ = AMF_VARIANT_SIZE;
        s.value.sizeValue = AMFSize { width, height };
        s
    }

    pub fn rate(num: u32, den: u32) -> AMFVariantStruct {
        let mut s = AMFVariantStruct::empty();
        s.type_ = AMF_VARIANT_RATE;
        s.value.rateValue = AMFRate { num, den };
        s
    }
}

/// A table entry this crate never calls.
pub type Unused = *const c_void;

/// `AMFInterface`: every AMF object starts with its table.
#[repr(C)]
pub struct AMFInterface {
    pub pVtbl: *const AMFInterfaceVtbl,
}

#[repr(C)]
pub struct AMFInterfaceVtbl {
    pub Acquire: unsafe extern "system" fn(this: *mut AMFInterface) -> amf_long,
    pub Release: unsafe extern "system" fn(this: *mut AMFInterface) -> amf_long,
    pub QueryInterface: unsafe extern "system" fn(this: *mut AMFInterface, iid: *const AMFGuid, out: *mut *mut c_void) -> AMF_RESULT,
}

/// The `AMFInterface` and `AMFPropertyStorage` entries every table below starts with, for an
/// object of type `T`.
#[repr(C)]
pub struct PropertyStorageVtbl<T> {
    pub Acquire: unsafe extern "system" fn(this: *mut T) -> amf_long,
    pub Release: unsafe extern "system" fn(this: *mut T) -> amf_long,
    pub QueryInterface: unsafe extern "system" fn(this: *mut T, iid: *const AMFGuid, out: *mut *mut c_void) -> AMF_RESULT,
    pub SetProperty: unsafe extern "system" fn(this: *mut T, name: *const wchar, value: AMFVariantStruct) -> AMF_RESULT,
    pub GetProperty: unsafe extern "system" fn(this: *mut T, name: *const wchar, value: *mut AMFVariantStruct) -> AMF_RESULT,
    pub HasProperty: Unused,
    pub GetPropertyCount: Unused,
    pub GetPropertyAt: Unused,
    pub Clear: Unused,
    pub AddTo: Unused,
    pub CopyTo: Unused,
    pub AddObserver: Unused,
    pub RemoveObserver: Unused,
}

/// The `AMFData` entries that follow the property storage ones in `AMFData`, `AMFSurface` and
/// `AMFBuffer`.
#[repr(C)]
pub struct DataVtbl<T> {
    pub GetMemoryType: Unused,
    pub Duplicate: Unused,
    pub Convert: Unused,
    pub Interop: Unused,
    pub GetDataType: Unused,
    pub IsReusable: Unused,
    pub SetPts: unsafe extern "system" fn(this: *mut T, pts: amf_pts),
    pub GetPts: unsafe extern "system" fn(this: *mut T) -> amf_pts,
    pub SetDuration: unsafe extern "system" fn(this: *mut T, duration: amf_pts),
    pub GetDuration: Unused,
}

#[repr(C)]
pub struct AMFData {
    pub pVtbl: *const AMFDataVtbl,
}

#[repr(C)]
pub struct AMFDataVtbl {
    pub storage: PropertyStorageVtbl<AMFData>,
    pub data: DataVtbl<AMFData>,
}

#[repr(C)]
pub struct AMFPlane {
    pub pVtbl: *const AMFPlaneVtbl,
}

#[repr(C)]
pub struct AMFPlaneVtbl {
    pub Acquire: Unused,
    pub Release: Unused,
    pub QueryInterface: Unused,
    pub GetType: Unused,
    pub GetNative: unsafe extern "system" fn(this: *mut AMFPlane) -> *mut c_void,
    pub GetPixelSizeInBytes: Unused,
    pub GetOffsetX: Unused,
    pub GetOffsetY: Unused,
    pub GetWidth: unsafe extern "system" fn(this: *mut AMFPlane) -> i32,
    pub GetHeight: unsafe extern "system" fn(this: *mut AMFPlane) -> i32,
    pub GetHPitch: unsafe extern "system" fn(this: *mut AMFPlane) -> i32,
    pub GetVPitch: Unused,
    pub IsTiled: Unused,
}

#[repr(C)]
pub struct AMFSurface {
    pub pVtbl: *const AMFSurfaceVtbl,
}

#[repr(C)]
pub struct AMFSurfaceVtbl {
    pub storage: PropertyStorageVtbl<AMFSurface>,
    pub data: DataVtbl<AMFSurface>,
    pub GetFormat: Unused,
    pub GetPlanesCount: Unused,
    pub GetPlaneAt: Unused,
    pub GetPlane: unsafe extern "system" fn(this: *mut AMFSurface, plane: AMF_ENUM) -> *mut AMFPlane,
    pub GetFrameType: Unused,
    pub SetFrameType: Unused,
    pub SetCrop: Unused,
    pub CopySurfaceRegion: Unused,
    pub AddObserver_Surface: Unused,
    pub RemoveObserver_Surface: Unused,
}

#[repr(C)]
pub struct AMFBuffer {
    pub pVtbl: *const AMFBufferVtbl,
}

#[repr(C)]
pub struct AMFBufferVtbl {
    pub storage: PropertyStorageVtbl<AMFBuffer>,
    pub data: DataVtbl<AMFBuffer>,
    pub SetSize: Unused,
    pub GetSize: unsafe extern "system" fn(this: *mut AMFBuffer) -> usize,
    pub GetNative: unsafe extern "system" fn(this: *mut AMFBuffer) -> *mut c_void,
    pub AddObserver_Buffer: Unused,
    pub RemoveObserver_Buffer: Unused,
}

#[repr(C)]
pub struct AMFContext {
    pub pVtbl: *const AMFContextVtbl,
}

#[repr(C)]
pub struct AMFContextVtbl {
    pub storage: PropertyStorageVtbl<AMFContext>,
    pub Terminate: unsafe extern "system" fn(this: *mut AMFContext) -> AMF_RESULT,
    pub InitDX9: Unused,
    pub GetDX9Device: Unused,
    pub LockDX9: Unused,
    pub UnlockDX9: Unused,
    pub InitDX11: unsafe extern "system" fn(this: *mut AMFContext, device: *mut c_void, version: AMF_ENUM) -> AMF_RESULT,
    pub GetDX11Device: Unused,
    pub LockDX11: Unused,
    pub UnlockDX11: Unused,
    pub InitOpenCL: Unused,
    pub GetOpenCLContext: Unused,
    pub GetOpenCLCommandQueue: Unused,
    pub GetOpenCLDeviceID: Unused,
    pub GetOpenCLComputeFactory: Unused,
    pub InitOpenCLEx: Unused,
    pub LockOpenCL: Unused,
    pub UnlockOpenCL: Unused,
    pub InitOpenGL: Unused,
    pub GetOpenGLContext: Unused,
    pub GetOpenGLDrawable: Unused,
    pub LockOpenGL: Unused,
    pub UnlockOpenGL: Unused,
    pub InitXV: Unused,
    pub GetXVDevice: Unused,
    pub LockXV: Unused,
    pub UnlockXV: Unused,
    pub InitGralloc: Unused,
    pub GetGrallocDevice: Unused,
    pub LockGralloc: Unused,
    pub UnlockGralloc: Unused,
    pub AllocBuffer: Unused,
    pub AllocSurface: unsafe extern "system" fn(
        this: *mut AMFContext,
        memory: AMF_ENUM,
        format: AMF_ENUM,
        width: i32,
        height: i32,
        out: *mut *mut AMFSurface,
    ) -> AMF_RESULT,
    pub AllocAudioBuffer: Unused,
    pub CreateBufferFromHostNative: Unused,
    pub CreateSurfaceFromHostNative: Unused,
    pub CreateSurfaceFromDX9Native: Unused,
    pub CreateSurfaceFromDX11Native: Unused,
    pub CreateSurfaceFromOpenGLNative: Unused,
    pub CreateSurfaceFromGrallocNative: Unused,
    pub CreateSurfaceFromOpenCLNative: Unused,
    pub CreateBufferFromOpenCLNative: Unused,
    pub GetCompute: Unused,
}

#[repr(C)]
pub struct AMFComponent {
    pub pVtbl: *const AMFComponentVtbl,
}

#[repr(C)]
pub struct AMFComponentVtbl {
    pub storage: PropertyStorageVtbl<AMFComponent>,
    pub GetPropertiesInfoCount: Unused,
    /// `GetPropertyInfo` and `GetPropertyInfoAt`, in the order MSVC builds the runtime with on
    /// Windows; neither is called.
    pub GetPropertyInfo: Unused,
    pub GetPropertyInfoAt: Unused,
    pub ValidateProperty: Unused,
    pub Init: unsafe extern "system" fn(this: *mut AMFComponent, format: AMF_ENUM, width: i32, height: i32) -> AMF_RESULT,
    pub ReInit: Unused,
    pub Terminate: unsafe extern "system" fn(this: *mut AMFComponent) -> AMF_RESULT,
    pub Drain: unsafe extern "system" fn(this: *mut AMFComponent) -> AMF_RESULT,
    pub Flush: Unused,
    pub SubmitInput: unsafe extern "system" fn(this: *mut AMFComponent, data: *mut AMFData) -> AMF_RESULT,
    pub QueryOutput: unsafe extern "system" fn(this: *mut AMFComponent, data: *mut *mut AMFData) -> AMF_RESULT,
    pub GetContext: Unused,
    pub SetOutputDataAllocatorCB: Unused,
    pub GetCaps: Unused,
    pub Optimize: Unused,
}

/// The factory is a process-wide singleton without reference counting.
#[repr(C)]
pub struct AMFFactory {
    pub pVtbl: *const AMFFactoryVtbl,
}

#[repr(C)]
pub struct AMFFactoryVtbl {
    pub CreateContext: unsafe extern "system" fn(this: *mut AMFFactory, out: *mut *mut AMFContext) -> AMF_RESULT,
    pub CreateComponent:
        unsafe extern "system" fn(this: *mut AMFFactory, context: *mut AMFContext, id: *const wchar, out: *mut *mut AMFComponent) -> AMF_RESULT,
    pub SetCacheFolder: Unused,
    pub GetCacheFolder: Unused,
    pub GetDebug: Unused,
    pub GetTrace: Unused,
    pub GetPrograms: Unused,
}

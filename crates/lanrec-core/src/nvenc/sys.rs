//! Raw FFI for the NVENC API.
//!
//! Generated at build time from `vendor/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h`
//! (MIT licensed, no NVIDIA account required). See `build.rs`.
//!
//! The runtime itself is `nvEncodeAPI64.dll`, which ships with the display driver and
//! is loaded dynamically at startup -- there is no link-time dependency on any SDK.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unused_imports,
    unnecessary_transmutes,
    // Generated bitfield accessors: bindgen writes them without safety docs and
    // with usize-to-isize offsets. Not ours to fix.
    clippy::missing_safety_doc,
    clippy::ptr_offset_with_cast,
    clippy::useless_transmute,
    // FFI signatures mirror the C API; the arity is not ours to choose.
    clippy::too_many_arguments
)]

include!(concat!(env!("OUT_DIR"), "/nvenc_sys.rs"));
include!(concat!(env!("OUT_DIR"), "/nvenc_guids.rs"));
include!(concat!(env!("OUT_DIR"), "/nvenc_versions.rs"));

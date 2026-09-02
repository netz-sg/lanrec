# Third-party code

## vendor/nv-codec-headers

The NVENC API headers under `vendor/nv-codec-headers/` come from the FFmpeg
project's [nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers)
repository, corresponding to NVIDIA Video Codec SDK **13.1.15**.

    Copyright (c) 2010-2026 NVIDIA Corporation

They are distributed under the MIT license; the full grant is at the top of
`include/ffnvcodec/nvEncodeAPI.h`.

They are vendored rather than fetched at build time so a clone builds without
network access, and because the exact SDK version determines the struct layouts
that `build.rs` generates bindings from.

Only the headers are used. The NVENC runtime itself is `nvEncodeAPI64.dll`,
which ships with the NVIDIA display driver and is loaded at runtime -- lanrec has
no link-time dependency on any NVIDIA SDK, and no NVIDIA account is needed to
build it.

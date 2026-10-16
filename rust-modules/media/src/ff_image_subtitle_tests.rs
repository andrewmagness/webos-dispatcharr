//! Subtitles through the FFmpeg the app SHIPS — the component list is the seam under test.
//!
//! Every other `ff` test stays off `av_*`, because the host suite has no FFmpeg to call. This one
//! cannot: what it grades is a property of `ci/build-ffmpeg.sh`'s configure line, which no pure
//! function can see. So it loads the host build of that same FFmpeg (`HOST=1 ci/build-ffmpeg.sh`,
//! staged into `pkg/` by `make check-ffmpeg`) and is `#[ignore]`d everywhere else — an honest
//! "not run" in `make check`, never a pass that executed nothing.

use super::*;

/// Where the host FFmpeg was staged: `PLX_FFMPEG_DIR` (what `make check-ffmpeg` passes), else the
/// checkout's own `pkg/`.
pub(super) fn host_ffmpeg_dir() -> std::path::PathBuf {
    std::env::var_os("PLX_FFMPEG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pkg"))
}

/// Bind the three libraries exactly as `load_libraries` does — dependency order, one directory —
/// and hold the build to the ABI table this file's offsets were checked against.
pub(super) fn bind_host_ffmpeg() {
    let dir = host_ffmpeg_dir();
    for (what, verdict) in [
        ("avutil", avutil::load(Some(&dir))),
        ("avcodec", avcodec::load(Some(&dir))),
        ("avformat", avformat::load(Some(&dir))),
    ] {
        assert!(
            matches!(verdict, plx_base::dynlib::Loaded::Ok(_)),
            "{what}: no host FFmpeg in {} — run `make check-ffmpeg`",
            dir.display()
        );
    }
    let majors = unsafe {
        (
            avformat_version() >> 16,
            avcodec_version() >> 16,
            avutil_version() >> 16,
        )
    };
    assert_eq!(
        majors,
        (63, 63, 61),
        "the staged FFmpeg is not the one ff.rs is built for"
    );
}


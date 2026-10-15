//! MPEG-TS/AAC timestamp handling: in-band parameter-set recovery and ADTS
//! frame-clock backfill when the container timestamp is missing or unreliable.

use super::*;
#[allow(unused_imports)]
use super::test_support::*;

// -- MPEG-TS elementary-stream framing -----------------------------------------------

#[test]
fn a_ts_idr_keeps_in_band_parameter_sets_without_avcc_conversion() {
    let packet = [
        0, 0, 0, 1, 0x67, 0x42, 0x00, // SPS
        0, 0, 1, 0x68, 0xce, // PPS
        0, 0, 0, 1, 0x65, 0xaa, // IDR
    ];
    let mut sets = H264ParamSets::default();
    let mut out = Vec::new();
    assert_eq!(ts_h264_access_unit(&packet, &mut sets, &mut out), Ok(true));
    assert_eq!(out, packet);
    assert!(!sets.sps.is_empty());
    assert!(!sets.pps.is_empty());
}

#[test]
fn a_leading_missing_aac_timestamp_is_backfilled_from_frame_duration() {
    let mut stamps = [
        AudioStamp {
            au: 3,
            raw_ns: None,
            duration_ns: Some(21_333_333),
        },
        AudioStamp {
            au: 4,
            raw_ns: Some(900_000_000),
            duration_ns: Some(21_333_333),
        },
        AudioStamp {
            au: 5,
            raw_ns: None,
            duration_ns: Some(21_333_333),
        },
    ];
    assert_eq!(resolve_audio_stamps(&mut stamps), Ok(2));
    assert_eq!(stamps[0].raw_ns, Some(878_666_667));
    assert_eq!(stamps[2].raw_ns, Some(921_333_333));
}

#[test]
fn a_timestamp_free_aac_segment_uses_only_its_frame_clock() {
    let mut stamps = [
        AudioStamp {
            au: 0,
            raw_ns: None,
            duration_ns: Some(20_000_000),
        },
        AudioStamp {
            au: 1,
            raw_ns: None,
            duration_ns: Some(20_000_000),
        },
        AudioStamp {
            au: 2,
            raw_ns: None,
            duration_ns: Some(20_000_000),
        },
    ];
    assert_eq!(resolve_audio_stamps(&mut stamps), Ok(3));
    assert_eq!(
        stamps.iter().map(|stamp| stamp.raw_ns).collect::<Vec<_>>(),
        [Some(0), Some(20_000_000), Some(40_000_000)]
    );
}

#[test]
fn an_unanchored_aac_timestamp_hole_fails_closed() {
    let mut stamps = [
        AudioStamp {
            au: 0,
            raw_ns: Some(0),
            duration_ns: None,
        },
        AudioStamp {
            au: 1,
            raw_ns: None,
            duration_ns: None,
        },
    ];
    assert_eq!(
        resolve_audio_stamps(&mut stamps),
        Err("AAC timestamp hole has no duration anchor")
    );
}

#[test]
fn a_later_ts_idr_recovers_cached_parameter_sets() {
    let first = [0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3];
    let later = [0, 0, 1, 0x65, 4];
    let mut sets = H264ParamSets::default();
    let mut out = Vec::new();
    ts_h264_access_unit(&first, &mut sets, &mut out).unwrap();
    assert_eq!(ts_h264_access_unit(&later, &mut sets, &mut out), Ok(true));
    assert!(out.starts_with(&[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2]));
    assert!(out.ends_with(&later));
}

#[test]
fn a_ts_idr_without_any_parameter_sets_is_rejected() {
    let mut sets = H264ParamSets::default();
    let mut out = Vec::new();
    assert_eq!(
        ts_h264_access_unit(&[0, 0, 1, 0x65, 4], &mut sets, &mut out),
        Err("IDR has no in-band or cached SPS")
    );
}

#[test]
fn adts_detection_accepts_a_real_header_but_not_arbitrary_ff_bytes() {
    let mut frame = adts_header(3, 2, 11).to_vec();
    frame.extend_from_slice(&[0; 11]);
    assert!(packet_has_adts(&frame));
    assert!(!packet_has_adts(&[0xff, 0x00, 0, 0, 0, 0, 0]));
    assert_eq!(adts_duration_ns(&frame), Some(21_333_333));
    assert_eq!(adts_duration_ns(&[0xff, 0x00, 0, 0, 0, 0, 0]), None);
}

/// The progressive path's carry: a TS PES stamps its first AAC frame and leaves the rest untimed.
/// Measured on a live Dispatcharr channel before the carry existed: `feed a#1 fed=1400000000`, then
/// `fed=0` for every frame after it, and the audio lane never fed again.
#[test]
fn untimed_aac_frames_continue_from_the_frame_before() {
    let mut c = AudioCarry::default();
    let d = Some(21_333_333);
    assert_eq!(c.stamp(Some(1_400_000_000), d), 1_400_000_000);
    assert_eq!(c.stamp(None, d), 1_421_333_333);
    assert_eq!(c.stamp(None, d), 1_442_666_666);
    assert_eq!(c.stamp(Some(1_613_333_333), d), 1_613_333_333, "a PES timestamp wins");
    assert_eq!(c.stamp(None, d), 1_634_666_666);
}

#[test]
fn an_untimed_frame_with_nothing_to_carry_reads_as_zero() {
    let mut c = AudioCarry::default();
    assert_eq!(c.stamp(None, Some(21_333_333)), 0, "no anchor yet: the old reading");
    let mut c = AudioCarry::default();
    c.stamp(Some(5_000), None);
    assert_eq!(c.stamp(None, None), 0, "no duration: nothing to carry");
}

#[test]
fn adts_framed_packets_are_recognised() {
    // FF F1: sync, MPEG-4, layer 0, no CRC. The TS AAC the live channels carry.
    assert!(packet_has_adts(&[0xFF, 0xF1, 0x50, 0x80, 0x40, 0x1F, 0xFC]));
    assert!(!packet_has_adts(&[0x21, 0x10, 0x05, 0x00, 0x00, 0x00, 0x00]), "raw AAC from MKV/MP4");
}

/// A live channel's open waits out a one-stream source freeing the slot the last channel held;
/// nothing else is retried, and the wait ends.
#[test]
fn only_a_live_channels_busy_open_is_retried_and_not_forever() {
    assert!(live_busy_retry(true, 503, 0));
    assert!(live_busy_retry(true, 503, LIVE_BUSY_RETRIES - 1));
    assert!(!live_busy_retry(true, 503, LIVE_BUSY_RETRIES), "the wait ends");
    assert!(!live_busy_retry(false, 503, 0), "a file's 503 is the server's answer");
    assert!(!live_busy_retry(true, 404, 0), "a missing channel is not busy");
    assert!(!live_busy_retry(true, 0, 0), "nor is a server that never answered");
}

/// A live channel's failover stream may carry AC-3 where the Load declared AAC; that is refused so
/// the application re-tunes with the stream's own codecs.
#[test]
fn audio_that_is_not_what_the_load_declared_is_a_mismatch() {
    assert!(!audio_mismatch(3, "aac", true), "AAC declared, AAC found");
    assert!(audio_mismatch(3, "ac3", true), "AAC declared, AC-3 found");
    assert!(audio_mismatch(1, "aac", true));
    assert!(audio_mismatch(3, "mp2", true), "a codec the Load cannot name is never what it declared");
    assert!(!audio_mismatch(3, "ac3", false), "not feeding audio: nothing to mismatch");
    assert!(!audio_mismatch(0, "ac3", true), "no audio declared");
    assert!(!audio_mismatch(3, "", true), "no audio stream");
}

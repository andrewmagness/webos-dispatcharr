//! MPEG-TS/AAC timestamp handling: in-band parameter-set recovery and ADTS
//! frame-clock backfill when the container timestamp is missing or unreliable.

use super::*;
#[allow(unused_imports)]
use super::test_support::*;

// -- MPEG-TS elementary-stream framing -----------------------------------------------

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
    assert!(audio_mismatch(3, "mp2", true), "AAC declared, MP2 found");
    assert!(!audio_mismatch(5, "mp2", true), "MP2 is what an MP3 declaration plays");
    assert!(audio_mismatch(3, "opus", true), "a codec no Load can name is never what it declared");
    assert!(!audio_mismatch(3, "ac3", false), "not feeding audio: nothing to mismatch");
    assert!(!audio_mismatch(0, "ac3", true), "no audio declared");
    assert!(!audio_mismatch(3, "", true), "no audio stream");
}

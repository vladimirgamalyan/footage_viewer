//! An append-only record of the footage this tool is pointed at.
//!
//! The log (`logging.rs`) captures what one session *did* and is rotated away
//! after twenty runs. This is the opposite kind of file: one line per clip read,
//! kept for good, describing what the clip *is* — container, codec, resolution,
//! bitrate, and above all how its keyframes are laid out — together with what
//! reading it cost on that machine.
//!
//! It exists because the decode constants in `media` are all bets on the
//! material: which frame sizes go to the GPU, how far a forward scrub decodes in
//! place rather than seeking, how much memory a scrub's frames are worth. Every
//! one of those was set against a handful of dev fixtures and a single tester's
//! log, and the fixtures are known not to match the real archive (the 4K fixture
//! keyframes every 0.48 s where the tester's camera does so every ~1 s). A grid
//! pass already learns all of this per clip at no extra cost, so keeping the
//! records turns "optimize for the real footage" into reading a file rather than
//! guessing. See `docs/adr/0013`.

use std::fs::OpenOptions;
use std::io::Write;

use footage_viewer_media as media;

use crate::logging;

/// Name of the dataset, written next to the executable beside the logs.
const STATS_NAME: &str = "footage_viewer_clips.jsonl";

/// Append one clip's stats to the dataset.
///
/// Best-effort, exactly like the log: an unwritable folder means no dataset, not
/// a failed pass. Called from grid workers, so two clips (the open and its
/// read-ahead) can land at once — each line is written with a single `write_all`
/// to a handle opened for append, which Windows serializes against other
/// appenders, so concurrent records interleave as whole lines or not at all.
pub fn record(s: &media::ClipStats) {
    let path = logging::log_dir().join(STATS_NAME);
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = f.write_all(line(s, &logging::utc_timestamp()).as_bytes());
}

/// One clip as a JSON line.
///
/// Hand-formatted for the same reason the log's timestamp is (ADR-0008): `serde`
/// and `serde_json` are not in the active dependency tree, and one `format!` is
/// cheaper than making the offline build depend on them. JSON Lines rather than
/// CSV so fields can be added later without stranding the records already
/// collected.
///
/// That tolerance is also what lets a field be *renamed* when what it measures
/// changes, which is the only safe way to change one. `scan_ms`/`scan_mb_s` were
/// `demux_ms`/`read_mb_s` until ADR-0015 made the grid seek to the keyframes
/// instead of reading past them: the old names describe a sequential read of the
/// whole file, the new ones a skip-scan of a third of it, and the two are not
/// comparable. Records from before it keep the old names and the old meaning, so
/// a reader gets two sparse columns rather than one dense misleading one.
///
/// Nothing here is escaped, because nothing here can need it: the text fields are
/// either libav identifiers (`hevc`, `yuv420p10le`) or a Windows file name, and
/// Windows forbids every character JSON would care about (`"`, `\`, controls) in
/// a name. Non-ASCII passes through as-is, which JSON allows. Adding a field
/// carrying free text — an error message, a path — would break that and want a
/// real encoder.
fn line(s: &media::ClipStats, ts: &str) -> String {
    format!(
        "{{\"ts\":\"{ts}\",\"file\":\"{file}\",\"size_mb\":{size_mb:.1},\
         \"container\":\"{container}\",\"codec\":\"{codec}\",\"profile\":\"{profile}\",\
         \"level\":{level},\"w\":{w},\"h\":{h},\"pix_fmt\":\"{pix_fmt}\",\
         \"fps\":{fps:.3},\"dur_s\":{dur_s:.3},\"mbit_s\":{mbit_s:.1},\"has_b\":{has_b},\
         \"packets\":{packets},\"keyframes\":{keyframes},\
         \"gop_frames\":{{\"min\":{gf_min:.0},\"mean\":{gf_mean:.1},\"max\":{gf_max:.0}}},\
         \"gop_s\":{{\"min\":{gs_min:.3},\"mean\":{gs_mean:.3},\"max\":{gs_max:.3}}},\
         \"key_share_pct\":{key_share:.1},\"hw\":\"{hw}\",\
         \"grid_ms\":{grid_ms:.0},\"open_ms\":{open_ms:.0},\"setup_ms\":{setup_ms:.0},\
         \"scan_ms\":{scan_ms:.0},\"decode_ms\":{decode_ms:.0},\"convert_ms\":{convert_ms:.0},\
         \"scan_mb_s\":{scan_mb_s:.0}}}\n",
        file = s.file,
        size_mb = s.size_bytes as f64 / 1e6,
        container = s.container,
        codec = s.codec,
        profile = s.profile,
        level = s.level,
        w = s.width,
        h = s.height,
        pix_fmt = s.pix_fmt,
        fps = s.fps,
        dur_s = s.duration_s,
        mbit_s = s.video_mbit_s,
        has_b = s.has_b_frames,
        packets = s.packets,
        keyframes = s.keyframes,
        gf_min = s.gop_frames_min,
        gf_mean = s.gop_frames_mean,
        gf_max = s.gop_frames_max,
        gs_min = s.gop_s_min,
        gs_mean = s.gop_s_mean,
        gs_max = s.gop_s_max,
        key_share = s.key_share_pct,
        hw = if s.hw_decode { "GPU" } else { "CPU" },
        grid_ms = s.grid_ms,
        open_ms = s.open_ms,
        setup_ms = s.setup_ms,
        scan_ms = s.scan_ms,
        decode_ms = s.decode_ms,
        convert_ms = s.convert_ms,
        scan_mb_s = s.scan_mb_s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> media::ClipStats {
        media::ClipStats {
            file: "C0042.MP4".to_owned(),
            size_bytes: 1_842_300_000,
            container: "mov,mp4,m4a,3gp,3g2,mj2".to_owned(),
            codec: "hevc".to_owned(),
            profile: "HEVC(Main10)".to_owned(),
            level: 153,
            width: 3840,
            height: 2160,
            pix_fmt: "yuv420p10le".to_owned(),
            fps: 29.97,
            duration_s: 312.4,
            video_mbit_s: 47.2,
            has_b_frames: true,
            packets: 9360,
            keyframes: 312,
            gop_frames_min: 30.0,
            gop_frames_mean: 30.0,
            gop_frames_max: 30.0,
            gop_s_min: 1.0,
            gop_s_mean: 1.0,
            gop_s_max: 1.0,
            key_share_pct: 18.4,
            hw_decode: true,
            grid_ms: 4820.0,
            open_ms: 31.0,
            setup_ms: 12.0,
            scan_ms: 3110.0,
            decode_ms: 1520.0,
            convert_ms: 160.0,
            scan_mb_s: 592.0,
        }
    }

    /// The record is one line of JSON. Both matter: a line per clip is what makes
    /// the file appendable from a worker and readable by `jq`/pandas, and the
    /// brace-escaping in a `format!` this size is easy to get wrong in a way
    /// nothing else would catch — a malformed line is only noticed when the whole
    /// dataset fails to parse, long after the footage is gone.
    #[test]
    fn record_is_one_json_line() {
        let line = line(&sample(), "2026-07-16 10:22:31.123Z");

        assert!(line.ends_with('\n'), "record must be one line: {line}");
        assert_eq!(line.matches('\n').count(), 1, "record spans lines: {line}");
        assert!(line.starts_with(r#"{"ts":"2026-07-16 10:22:31.123Z","file":"C0042.MP4""#));
        assert!(line.trim_end().ends_with("\"scan_mb_s\":592}"), "{line}");
        // Braces balance, i.e. the nested objects opened and closed as intended.
        assert_eq!(line.matches('{').count(), line.matches('}').count(), "{line}");
    }

    /// The keyframe layout is the reason the dataset exists, so it is spelled out
    /// rather than left to the reader: frames and seconds are separate objects,
    /// and both carry the spread, not just a mean.
    #[test]
    fn record_carries_the_keyframe_layout() {
        let mut s = sample();
        // A variable GOP — the case the grid's sampler assumes away, and the one
        // the dataset is meant to reveal.
        s.gop_frames_min = 12.0;
        s.gop_frames_mean = 28.5;
        s.gop_frames_max = 30.0;
        s.gop_s_min = 0.4;
        s.gop_s_mean = 0.95;
        s.gop_s_max = 1.001;
        let line = line(&s, "2026-07-16 10:22:31.123Z");

        assert!(
            line.contains(r#""gop_frames":{"min":12,"mean":28.5,"max":30}"#),
            "{line}"
        );
        assert!(
            line.contains(r#""gop_s":{"min":0.400,"mean":0.950,"max":1.001}"#),
            "{line}"
        );
    }

    /// A CPU decode is recorded as such: which decoder libav actually used is the
    /// one thing a log line cannot be reconstructed from later.
    #[test]
    fn record_names_the_decoder_that_ran() {
        let mut s = sample();
        s.hw_decode = false;
        assert!(line(&s, "t").contains(r#""hw":"CPU""#));

        s.hw_decode = true;
        assert!(line(&s, "t").contains(r#""hw":"GPU""#));
    }
}

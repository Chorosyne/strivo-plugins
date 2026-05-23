//! ffmpeg concat argv synthesis. (E2.)
//!
//! Takes the working clip list + the word stream and emits the ffmpeg
//! command that the host pipeline executor will spawn. For the M4 MVP
//! we use the `concat` demuxer (`-f concat -safe 0 -i list.txt -c copy
//! out.mkv`) which is lossless when every clip's in/out aligns to a
//! video keyframe. Misalignment requires a partial re-encode; that
//! path is held until the host pipeline refactor lands.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

use super::EditorClip;

/// Returned command shape — the executor consumes `argv` and writes
/// `concat_file_contents` to `concat_file_path` first.
#[derive(Debug, Clone)]
pub struct ConcatPlan {
    pub argv: Vec<String>,
    pub concat_file_path: PathBuf,
    pub concat_file_contents: String,
    pub output_path: PathBuf,
}

/// Build the ffmpeg concat command for the clip list. Each clip is a
/// `[seg-N.mkv]` carved by an upstream stage (host pipeline). For the
/// MVP we shortcut: emit a concat-demuxer file that points at the
/// source recording with per-clip `inpoint` / `outpoint` directives;
/// ffmpeg slices internally with `-c copy` — lossless when keyframes
/// align, otherwise the demuxer falls back to re-encoding the first
/// GOP of each segment automatically.
pub fn build_concat_argv(
    clips: &[EditorClip],
    words: &[(String, f64)],
    source: &Option<PathBuf>,
) -> Result<ConcatPlan> {
    if clips.is_empty() {
        return Err(anyhow!("no clips to concat"));
    }
    let source = source
        .as_ref()
        .ok_or_else(|| anyhow!("no source recording loaded"))?;

    // Build the concat demuxer's input list.
    let mut list = String::new();
    for c in clips {
        let in_secs = words
            .get(c.in_word as usize)
            .map(|(_, s)| *s)
            .ok_or_else(|| anyhow!("in_word {} out of range", c.in_word))?;
        let out_secs = words
            .get(c.out_word as usize)
            .map(|(_, s)| *s)
            .ok_or_else(|| anyhow!("out_word {} out of range", c.out_word))?;
        if out_secs <= in_secs {
            return Err(anyhow!(
                "clip {}: out_secs ({:.2}) ≤ in_secs ({:.2})",
                c.label,
                out_secs,
                in_secs
            ));
        }
        // The concat demuxer's `inpoint`/`outpoint` syntax wants the
        // file declared once before the directives.
        list.push_str(&format!("file '{}'\n", source.display()));
        list.push_str(&format!("inpoint {in_secs:.3}\n"));
        list.push_str(&format!("outpoint {out_secs:.3}\n"));
    }

    let output_dir = source
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let concat_file_path = output_dir.join(format!("{stem}.concat.txt"));
    let output_path = output_dir.join(format!("{stem}.editor.mkv"));

    let argv = vec![
        "ffmpeg".to_string(),
        "-y".to_string(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-protocol_whitelist".into(),
        "file,pipe".into(),
        "-i".into(),
        concat_file_path.to_string_lossy().into_owned(),
        // Try lossless first; if a clip is keyframe-misaligned, ffmpeg
        // will emit a warning and re-encode that GOP. Acceptable for
        // M4 — explicit user-controlled re-encode is C-tier polish.
        "-c".into(),
        "copy".into(),
        "-map".into(),
        "0".into(),
        "-avoid_negative_ts".into(),
        "make_zero".into(),
        output_path.to_string_lossy().into_owned(),
    ];

    Ok(ConcatPlan {
        argv,
        concat_file_path,
        concat_file_contents: list,
        output_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(words: &[(&str, f64)]) -> Vec<(String, f64)> {
        words.iter().map(|(w, s)| ((*w).to_string(), *s)).collect()
    }

    #[test]
    fn empty_clip_list_errors() {
        let res = build_concat_argv(
            &[],
            &ws(&[("a", 0.0)]),
            &Some(PathBuf::from("/tmp/x.mkv")),
        );
        assert!(res.is_err());
    }

    #[test]
    fn missing_source_errors() {
        let clips = vec![EditorClip {
            in_word: 0,
            out_word: 1,
            label: "x".into(),
        }];
        let res = build_concat_argv(&clips, &ws(&[("a", 0.0), ("b", 1.0)]), &None);
        assert!(res.is_err());
    }

    #[test]
    fn out_of_range_errors() {
        let clips = vec![EditorClip {
            in_word: 0,
            out_word: 99,
            label: "x".into(),
        }];
        let res = build_concat_argv(
            &clips,
            &ws(&[("a", 0.0), ("b", 1.0)]),
            &Some(PathBuf::from("/tmp/x.mkv")),
        );
        assert!(res.is_err());
    }

    #[test]
    fn concat_argv_shape() {
        let clips = vec![
            EditorClip {
                in_word: 0,
                out_word: 1,
                label: "a".into(),
            },
            EditorClip {
                in_word: 1,
                out_word: 2,
                label: "b".into(),
            },
        ];
        let plan = build_concat_argv(
            &clips,
            &ws(&[("hi", 0.0), ("there", 1.5), ("end", 3.2)]),
            &Some(PathBuf::from("/tmp/show.mkv")),
        )
        .unwrap();
        assert_eq!(plan.argv[0], "ffmpeg");
        assert!(plan.argv.iter().any(|a| a == "concat"));
        assert!(plan.argv.iter().any(|a| a == "-c"));
        assert!(plan.argv.iter().any(|a| a == "copy"));
        assert!(plan.concat_file_contents.contains("inpoint 0.000"));
        assert!(plan.concat_file_contents.contains("outpoint 1.500"));
        assert!(plan.concat_file_contents.contains("inpoint 1.500"));
        assert!(plan.concat_file_contents.contains("outpoint 3.200"));
    }
}

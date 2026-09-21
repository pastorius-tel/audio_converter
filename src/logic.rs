//! GUI(egui/eframe)に依存しない純粋ロジックをまとめたモジュール。
//!
//! ここに置く関数は `cargo test` で単体テストできる(≒外部I/Oや
//! 画面描画を含まない)ことを条件にする。バグを仕込みやすい
//! 「文字列パース」「パス比較」「ファイル名衝突回避」をここに集約し、
//! main.rs 側は薄いI/Oラッパーに徹する。

use std::path::{Path, PathBuf};

/// `ffmpeg -i <file>` の stderr から `Duration: HH:MM:SS.xx` 行を探し、
/// 秒数(f64)に変換する。見つからない/形式が違う/`N/A` の場合は `None`。
///
/// ffmpeg のこの種のログ行は言語(ロケール)設定によらず常に英語で
/// 出力されるため、日本語Windows環境でも同じフォーマットで解析できる。
pub fn parse_ffmpeg_duration(stderr: &str) -> Option<f64> {
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Duration:") {
            // 例: "  00:01:23.45, start: 0.000000, bitrate: 128 kb/s"
            let dur_part = rest.split(',').next()?.trim();
            if dur_part.eq_ignore_ascii_case("N/A") {
                return None;
            }
            let parts: Vec<&str> = dur_part.split(':').collect();
            if parts.len() == 3 {
                let h: f64 = parts[0].trim().parse().ok()?;
                let m: f64 = parts[1].trim().parse().ok()?;
                let s: f64 = parts[2].trim().parse().ok()?;
                if h.is_finite() && m.is_finite() && s.is_finite() {
                    return Some(h * 3600.0 + m * 60.0 + s);
                }
            }
        }
    }
    None
}

/// 与えられた文字列が「PATHで解決される裸のコマンド名」(例: `ffmpeg`,
/// `ffmpeg.exe`)かどうかを判定する。
///
/// 絶対パスや、区切り文字(`/` や Windows の `\`)、ドライブレター(`C:`)を
/// 含む文字列は「具体的な場所を指している」とみなし false を返す。
/// この判定は「ファイル存在チェックをスキップして信頼してよいか」の
/// 分岐に使う。裸のコマンド名は実行時に PATH 上で解決されるため、
/// 現在のカレントディレクトリを基準にした `Path::exists()` では
/// 正しく検証できない。
///
/// 意図的に `std::path::Path` を使わず文字列としてチェックしている:
/// `Path` の絶対パス判定はコンパイル対象OSに依存するため
/// (例: Unix向けビルドでは `C:\...` はただの1コンポーネント扱いになる)、
/// 「Windowsパス文字列」を対象OSによらず一貫して判定できるようにするため。
pub fn is_bare_command(value: &str) -> bool {
    !value.contains('/') && !value.contains('\\') && !value.contains(':')
}

/// 2つのパスが同一ファイルを指しているとみなせるかどうかを判定する。
///
/// Windows のファイルシステムは大文字小文字を区別しないため、
/// 同じファイルが異なる大文字小文字表記で2回選択された場合に
/// 別ファイルとして重複登録されるのを防ぐ。
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// 出力先ディレクトリ・ファイル名の元(stem)・タイムスタンプ・拡張子から
/// 出力ファイルパスを決定する。
///
/// 同名ファイルが既に存在する場合は " (2)", " (3)", ... を付けて
/// 上書きを回避する(ffmpeg 呼び出し側は `-y` で上書きを許可しているため、
/// パス生成側で重複を避けないとサイレントに上書きされてしまう)。
///
/// `exists_fn` を差し替え可能にすることで、実ファイルシステムに触れず
/// 単体テストできるようにしている。
pub fn unique_output_path_with<F: Fn(&Path) -> bool>(
    dir: &Path,
    stem: &str,
    timestamp: &str,
    ext: &str,
    exists_fn: F,
) -> PathBuf {
    let base = format!("{}_{}", stem, timestamp);
    let mut candidate = dir.join(format!("{}.{}", base, ext));
    let mut n = 2;
    while exists_fn(&candidate) {
        candidate = dir.join(format!("{} ({}).{}", base, n, ext));
        n += 1;
    }
    candidate
}

/// 実ファイルシステムを見て衝突回避する版(本体コードから使う実体)。
pub fn unique_output_path(dir: &Path, stem: &str, timestamp: &str, ext: &str) -> PathBuf {
    unique_output_path_with(dir, stem, timestamp, ext, |p| p.exists())
}

// ─────────────────────────────────────────────────────────────────────────────
// 単体テスト
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn duration_basic() {
        let stderr = "Input #0, wav, from 'x.wav':\n  Duration: 00:01:23.45, start: 0.000000, bitrate: 128 kb/s\n";
        let d = parse_ffmpeg_duration(stderr).unwrap();
        assert!((d - 83.45).abs() < 1e-6);
    }

    #[test]
    fn duration_hours() {
        let stderr = "  Duration: 01:02:03.00, start: 0.000000\n";
        let d = parse_ffmpeg_duration(stderr).unwrap();
        assert!((d - (3600.0 + 120.0 + 3.0)).abs() < 1e-6);
    }

    #[test]
    fn duration_na_returns_none() {
        let stderr = "  Duration: N/A, bitrate: N/A\n";
        assert_eq!(parse_ffmpeg_duration(stderr), None);
    }

    #[test]
    fn duration_missing_returns_none() {
        let stderr = "Input #0, wav, from 'x.wav':\n  Stream #0:0: Audio: pcm_s16le\n";
        assert_eq!(parse_ffmpeg_duration(stderr), None);
    }

    #[test]
    fn duration_malformed_returns_none() {
        // コロンの数が想定外
        let stderr = "  Duration: garbage, start: 0.000000\n";
        assert_eq!(parse_ffmpeg_duration(stderr), None);
    }

    #[test]
    fn duration_exactly_two_minutes_is_not_over() {
        // 「2分以内」は120秒ちょうどを含む(仕様の "以内" は inclusive)。
        let stderr = "  Duration: 00:02:00.00, start: 0.000000\n";
        let d = parse_ffmpeg_duration(stderr).unwrap();
        assert_eq!(d, 120.0);
        assert!(d <= 120.0);
    }

    #[test]
    fn bare_command_names_detected() {
        assert!(is_bare_command("ffmpeg"));
        assert!(is_bare_command("ffmpeg.exe"));
        assert!(!is_bare_command("./ffmpeg"));
        assert!(!is_bare_command("tools/ffmpeg.exe"));
        assert!(!is_bare_command("C:\\tools\\ffmpeg.exe"));
        assert!(!is_bare_command("/usr/bin/ffmpeg"));
    }

    #[test]
    fn paths_equal_case_sensitivity() {
        let a = Path::new("C:\\Data\\greeting.wav");
        let b = Path::new("c:\\data\\GREETING.WAV");
        if cfg!(windows) {
            assert!(paths_equal(a, b));
        } else {
            assert!(!paths_equal(a, b));
        }
        // 完全に違うパスは常に false
        assert!(!paths_equal(Path::new("a.wav"), Path::new("b.wav")));
    }

    #[test]
    fn unique_output_path_no_collision() {
        let existing: HashSet<PathBuf> = HashSet::new();
        let p = unique_output_path_with(
            Path::new("/out"),
            "greeting",
            "20260427_143052",
            "wav",
            |p| existing.contains(p),
        );
        assert_eq!(p, PathBuf::from("/out/greeting_20260427_143052.wav"));
    }

    #[test]
    fn unique_output_path_avoids_collision() {
        // 1回目・2回目の候補が「既に存在する」ことにして、3回目で確定させる
        let taken: HashSet<PathBuf> = [
            PathBuf::from("/out/greeting_20260427_143052.wav"),
            PathBuf::from("/out/greeting_20260427_143052 (2).wav"),
        ]
        .into_iter()
        .collect();

        let p = unique_output_path_with(
            Path::new("/out"),
            "greeting",
            "20260427_143052",
            "wav",
            |p| taken.contains(p),
        );
        assert_eq!(p, PathBuf::from("/out/greeting_20260427_143052 (3).wav"));
    }

    #[test]
    fn unique_output_path_different_stems_no_suffix() {
        // 拡張子や stem が異なれば衝突しない
        let taken: HashSet<PathBuf> = [PathBuf::from("/out/a_20260427_143052.wav")]
            .into_iter()
            .collect();
        let p = unique_output_path_with(Path::new("/out"), "b", "20260427_143052", "wav", |p| {
            taken.contains(p)
        });
        assert_eq!(p, PathBuf::from("/out/b_20260427_143052.wav"));
    }
}

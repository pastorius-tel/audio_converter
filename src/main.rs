#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 音声ファイル変換ツール
//!
//! 複数の業務用電話機/留守番電話システムが要求する音声形式に合わせて、
//! 一般的な音声ファイルを ffmpeg 経由で一括変換するための GUI アプリ。
//!
//! GUI(egui/eframe)に依存しない純粋ロジックは `logic` モジュールに
//! 切り出してあり、`cargo test` で単体テストできる。

mod logic;

use eframe::egui;
use logic::{is_bare_command, parse_ffmpeg_duration, paths_equal, unique_output_path};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ─────────────────────────────────────────────────────────────────────────────
// Mutex ヘルパー: ポイズン(パニック中のロック保持)を無視して復旧する。
//
// 標準の `Mutex::lock()` は、ロックを保持したスレッドがパニックすると
// 以降ずっと `Err` を返すようになる(ポイズニング)。本アプリではその
// エラーを `if let Ok(...)` で握りつぶし「ロック失敗 = false/何もしない」
// として扱っていたが、これは「変換中フラグ」等の状態が実際には
// 壊れていないにもかかわらず誤った既定値にフォールバックしてしまい、
// 最悪の場合ワーカースレッドが1つパニックしただけで多重変換や
// 状態不整合を招きうる。中身のデータ自体は壊れていないため、
// ポイズンを無視してそのまま復旧するのが妥当。
// ─────────────────────────────────────────────────────────────────────────────
trait LockExt<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T>;
}
impl<T> LockExt<T> for Mutex<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// プリセット定義
//
// ラベルには対象機器のメーカー名を含める(どの機器向けか一目で分かるように
// するため)。「録音時間の上限」のような機器固有の制約は
// `max_duration_secs()` でデータとして表現し、呼び出し側で
// `Preset::XxxYyy` と名前で特別扱いしない設計にしている
// (将来プリセットが増えても分岐が汚れない)。
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preset {
    /// タカコム留守番: μ-law / 8000Hz / 8bit / モノラル
    TakacomMuLaw,
    /// タカコム留守番: PCM / 44100Hz / 16bit / モノラル
    TakacomPcmMono,
    /// タカコム留守番: PCM / 44100Hz / 16bit / ステレオ
    TakacomPcmStereo,
    /// ナカヨ: AAC-LC / ADTS / 8000Hz / 24kbps / モノラル
    NakayoAac,
    /// 岩崎通信機: PCM / 8000Hz / 16bit / モノラル(録音時間 2 分以内の制約あり)
    IwatsuPcmLimited,
}

impl Preset {
    const ALL: [Preset; 5] = [
        Preset::TakacomMuLaw,
        Preset::TakacomPcmMono,
        Preset::TakacomPcmStereo,
        Preset::NakayoAac,
        Preset::IwatsuPcmLimited,
    ];

    /// UI上でグループ見出しとして表示するメーカー名。
    /// 直前のプリセットと同じ場合は呼び出し側で見出しを省略する。
    fn group(&self) -> &'static str {
        match self {
            Preset::TakacomMuLaw | Preset::TakacomPcmMono | Preset::TakacomPcmStereo => {
                "タカコム留守番"
            }
            Preset::NakayoAac => "ナカヨ",
            Preset::IwatsuPcmLimited => "岩崎通信機",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Preset::TakacomMuLaw => "μ-law 8kHz 8bit モノラル",
            Preset::TakacomPcmMono => "PCM 44.1kHz 16bit モノラル",
            Preset::TakacomPcmStereo => "PCM 44.1kHz 16bit ステレオ",
            Preset::NakayoAac => "AAC (ADTS) 8kHz 24kbps モノラル (LC)",
            Preset::IwatsuPcmLimited => "PCM 8kHz 16bit モノラル (録音2分以内)",
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            Preset::NakayoAac => "aac",
            _ => "wav",
        }
    }

    /// この機器向けプリセットに録音時間の上限があれば秒数を返す。
    /// なければ `None`(無制限)。
    fn max_duration_secs(&self) -> Option<f64> {
        match self {
            Preset::IwatsuPcmLimited => Some(120.0),
            _ => None,
        }
    }

    /// ffmpeg 用の引数列を生成する。
    ///
    /// すべてのプリセットが具体的な仕様を持つため、以前あった
    /// 「仕様未定 → None」という分岐は廃止し、常に `Vec<String>` を返す
    /// (Option でラップする理由がなくなっていたため型を単純化した)。
    fn ffmpeg_args(&self, input: &str, output: &str) -> Vec<String> {
        let s = |x: &str| x.to_string();
        match self {
            // μ-law / 8000Hz / 1ch (8bit はμ-lawサンプル幅で自動)
            Preset::TakacomMuLaw => vec![
                s("-y"),
                s("-i"),
                s(input),
                s("-acodec"),
                s("pcm_mulaw"),
                s("-ar"),
                s("8000"),
                s("-ac"),
                s("1"),
                s(output),
            ],
            // PCM s16le / 44100Hz / 1ch
            Preset::TakacomPcmMono => vec![
                s("-y"),
                s("-i"),
                s(input),
                s("-acodec"),
                s("pcm_s16le"),
                s("-ar"),
                s("44100"),
                s("-ac"),
                s("1"),
                s(output),
            ],
            // PCM s16le / 44100Hz / 2ch
            Preset::TakacomPcmStereo => vec![
                s("-y"),
                s("-i"),
                s(input),
                s("-acodec"),
                s("pcm_s16le"),
                s("-ar"),
                s("44100"),
                s("-ac"),
                s("2"),
                s(output),
            ],
            // AAC-LC / ADTS / 8kHz / 24kbps / 1ch / CRC無し (ffmpeg内蔵aac既定)
            Preset::NakayoAac => vec![
                s("-y"),
                s("-i"),
                s(input),
                s("-c:a"),
                s("aac"),
                s("-profile:a"),
                s("aac_low"),
                s("-b:a"),
                s("24k"),
                s("-ar"),
                s("8000"),
                s("-ac"),
                s("1"),
                s("-f"),
                s("adts"),
                s(output),
            ],
            // PCM s16le / 8000Hz / 1ch (= 128kbps)
            // ※録音時間の上限チェックは呼び出し側(start_conversion)で実施
            Preset::IwatsuPcmLimited => vec![
                s("-y"),
                s("-i"),
                s(input),
                s("-acodec"),
                s("pcm_s16le"),
                s("-ar"),
                s("8000"),
                s("-ac"),
                s("1"),
                s(output),
            ],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// アプリ本体
// ─────────────────────────────────────────────────────────────────────────────

/// スキップされたファイルとその理由。
#[derive(Clone)]
struct SkippedFile {
    path: PathBuf,
    reason: String,
}

struct App {
    files: Vec<PathBuf>,
    preset: Preset,
    output_dir: Option<PathBuf>,
    log: Arc<Mutex<Vec<String>>>,
    converting: Arc<Mutex<bool>>,
    progress: Arc<Mutex<(usize, usize)>>, // (done, total)
    ffmpeg_path: String,
    /// 上限時間超過、または長さ不明のためスキップされたファイル一覧
    skipped_files: Arc<Mutex<Vec<SkippedFile>>>,
    /// スキップ警告ダイアログ表示フラグ
    show_skip_dialog: Arc<Mutex<bool>>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            preset: Preset::TakacomMuLaw,
            output_dir: None,
            log: Arc::new(Mutex::new(Vec::new())),
            converting: Arc::new(Mutex::new(false)),
            progress: Arc::new(Mutex::new((0, 0))),
            ffmpeg_path: initial_ffmpeg_path(),
            skipped_files: Arc::new(Mutex::new(Vec::new())),
            show_skip_dialog: Arc::new(Mutex::new(false)),
        }
    }
}

fn push_log(log: &Arc<Mutex<Vec<String>>>, msg: impl Into<String>) {
    log.lock_ignore_poison().push(msg.into());
}

/// ファイル一覧に追加する際、大文字小文字だけが違う重複(Windowsでは
/// 同一ファイル)を弾きながら push する。
fn add_file_if_new(files: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !files.iter().any(|f| paths_equal(f, &candidate)) {
        files.push(candidate);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ffmpeg パスの永続化 / 自動検出
// ─────────────────────────────────────────────────────────────────────────────

/// 実行ファイルのあるフォルダを返す(取得不能なら None)
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|x| x.to_path_buf()))
}

/// 設定ファイル(平テキスト1行)のパス
fn config_file_path() -> Option<PathBuf> {
    exe_dir().map(|d| d.join("audio_converter_config.txt"))
}

/// 保存された設定を一切見ずに ffmpeg パスを自動検出する(真の既定値)。
/// 1) exe と同じフォルダに ffmpeg(.exe) があればそれを使う
/// 2) なければ "ffmpeg" (PATH 解決に任せる)
///
/// `initial_ffmpeg_path()` とは別関数にしているのは、UI の「既定」ボタン
/// (自動検出に戻す)からもこのロジックを使うため。以前は「既定」ボタンが
/// `initial_ffmpeg_path()` をそのまま呼んでいたが、それだと設定ファイルに
/// 有効なパスが保存されている限り常にそのパスが読み戻されてしまい、
/// 「自動検出に戻す」というボタンの説明どおりに動作しない不具合があった。
fn detect_default_ffmpeg_path() -> String {
    if let Some(d) = exe_dir() {
        let candidate = d.join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "ffmpeg".to_string()
}

/// 起動時の ffmpeg パス決定:
/// 1) 設定ファイルに保存された値があれば、それを使う
///    (「ffmpeg」のような PATH 解決前提の裸のコマンド名は
///    ファイル存在チェックをスキップして信頼する。具体的なパスの
///    場合のみ実在確認する)
/// 2) なければ `detect_default_ffmpeg_path()` の自動検出結果を使う
fn initial_ffmpeg_path() -> String {
    if let Some(cfg) = config_file_path() {
        if let Ok(s) = std::fs::read_to_string(&cfg) {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty()
                && (is_bare_command(&trimmed) || std::path::Path::new(&trimmed).exists())
            {
                return trimmed;
            }
        }
    }
    detect_default_ffmpeg_path()
}

/// ffmpeg パスを設定ファイルに保存(失敗は無視)
fn save_ffmpeg_path(path: &str) -> std::io::Result<()> {
    if let Some(cfg) = config_file_path() {
        std::fs::write(cfg, path)?;
    }
    Ok(())
}

/// 指定された値が有効な ffmpeg として実行できるか軽く検査する。
/// (呼び出し側で背景スレッドから呼ぶことを想定 — UIスレッドをブロックしないため)
fn check_ffmpeg(path: &str) -> bool {
    let mut cmd = Command::new(path);
    cmd.arg("-version");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// ffmpeg を実行して音声長(秒)を取得する。
/// 出力先を指定せずに呼ぶので ffmpeg 自体は exit code != 0 で終わるが、
/// stderr に "Duration: HH:MM:SS.xx" が出るのでそれをパースする。
/// パース処理本体は `logic::parse_ffmpeg_duration`(単体テスト済み)。
fn get_audio_duration(ffmpeg: &str, input: &str) -> Option<f64> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-i", input]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output().ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_ffmpeg_duration(&stderr)
}

impl App {
    fn start_conversion(&self) {
        // 二重起動防止
        {
            let mut c = self.converting.lock_ignore_poison();
            if *c {
                return;
            }
            *c = true;
        }

        let files = self.files.clone();
        let preset = self.preset;
        let output_dir = self.output_dir.clone();
        let log = self.log.clone();
        let converting = self.converting.clone();
        let progress = self.progress.clone();
        let ffmpeg_path = self.ffmpeg_path.clone();
        let skipped_files = self.skipped_files.clone();
        let show_skip_dialog = self.show_skip_dialog.clone();

        // 前回のスキップ一覧はクリア
        skipped_files.lock_ignore_poison().clear();
        *progress.lock_ignore_poison() = (0, files.len());

        thread::spawn(move || {
            for (idx, input) in files.iter().enumerate() {
                let stem = input
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("output_{}", idx));

                let out_dir = output_dir.clone().unwrap_or_else(|| {
                    input
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .to_path_buf()
                });

                if let Err(e) = std::fs::create_dir_all(&out_dir) {
                    push_log(&log, format!("出力フォルダを作成できません: {}", e));
                    *progress.lock_ignore_poison() = (idx + 1, files.len());
                    continue;
                }

                let input_s = input.to_string_lossy().into_owned();

                // ── 録音時間の上限チェック(該当プリセットのみ) ──
                // 上限を超える場合はもちろん、長さが取得できなかった場合も
                // 安全側に倒してスキップする(機器側がハード上限を持つため、
                // 「たぶん大丈夫」で送り込むと再生できないファイルを
                // 作ってしまう恐れがある)。
                if let Some(limit) = preset.max_duration_secs() {
                    match get_audio_duration(&ffmpeg_path, &input_s) {
                        Some(dur) if dur > limit => {
                            let reason = format!(
                                "上限 {:.0} 秒を超過 ({:02}:{:05.2})",
                                limit,
                                (dur as u64) / 60,
                                dur % 60.0
                            );
                            push_log(
                                &log,
                                format!(
                                    "[{}/{}] スキップ ({}): {}",
                                    idx + 1,
                                    files.len(),
                                    reason,
                                    input.display()
                                ),
                            );
                            skipped_files.lock_ignore_poison().push(SkippedFile {
                                path: input.clone(),
                                reason,
                            });
                            *progress.lock_ignore_poison() = (idx + 1, files.len());
                            continue;
                        }
                        Some(_) => { /* 上限内、続行 */ }
                        None => {
                            let reason = "録音時間を取得できず安全のためスキップ".to_string();
                            push_log(
                                &log,
                                format!(
                                    "[{}/{}] スキップ ({}): {}",
                                    idx + 1,
                                    files.len(),
                                    reason,
                                    input.display()
                                ),
                            );
                            skipped_files.lock_ignore_poison().push(SkippedFile {
                                path: input.clone(),
                                reason,
                            });
                            *progress.lock_ignore_poison() = (idx + 1, files.len());
                            continue;
                        }
                    }
                }

                // ── 出力ファイル名決定(同名衝突は自動的に回避) ──
                let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
                let out_path = unique_output_path(&out_dir, &stem, &timestamp, preset.extension());
                let output_s = out_path.to_string_lossy().into_owned();

                let args = preset.ffmpeg_args(&input_s, &output_s);

                push_log(
                    &log,
                    format!("[{}/{}] 変換: {}", idx + 1, files.len(), input.display()),
                );

                let mut cmd = Command::new(&ffmpeg_path);
                cmd.args(&args);
                #[cfg(windows)]
                cmd.creation_flags(CREATE_NO_WINDOW);

                match cmd.output() {
                    Ok(out) => {
                        if out.status.success() {
                            push_log(&log, format!("  ✓ 出力: {}", out_path.display()));
                        } else {
                            let err = String::from_utf8_lossy(&out.stderr);
                            let last = err
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("(詳細不明)");
                            push_log(&log, format!("  ✗ 失敗: {}", last));
                        }
                    }
                    Err(e) => {
                        push_log(
                            &log,
                            format!("  ✗ ffmpeg を実行できません: {} (パス確認)", e),
                        );
                    }
                }

                *progress.lock_ignore_poison() = (idx + 1, files.len());
            }

            push_log(&log, "── 全ファイルの処理が完了しました ──");

            let has_skipped = !skipped_files.lock_ignore_poison().is_empty();
            if has_skipped {
                *show_skip_dialog.lock_ignore_poison() = true;
            }

            *converting.lock_ignore_poison() = false;
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── ドラッグ&ドロップ受け取り ──
        // フォルダがドロップされた場合はそのまま追加せず、ログで理由を
        // 知らせてスキップする(フォルダを ffmpeg の -i に渡すと確実に
        // 変換失敗するため、ファイル一覧に混入させない)。
        ctx.input(|i| {
            for f in &i.raw.dropped_files {
                if let Some(path) = &f.path {
                    if path.is_dir() {
                        push_log(
                            &self.log,
                            format!(
                                "フォルダはドロップできません(中のファイルを個別にドロップしてください): {}",
                                path.display()
                            ),
                        );
                    } else {
                        add_file_if_new(&mut self.files, path.clone());
                    }
                }
            }
        });

        // ホバー中の視覚フィードバック
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("音声ファイル変換ツール");
            ui.separator();

            // ── ファイル選択 ──
            ui.horizontal(|ui| {
                if ui.button("ファイルを選択").clicked() {
                    if let Some(paths) = rfd::FileDialog::new()
                        .add_filter(
                            "音声ファイル",
                            &[
                                "wav", "mp3", "aac", "m4a", "ogg", "flac", "wma", "amr", "au",
                            ],
                        )
                        .add_filter("すべてのファイル", &["*"])
                        .pick_files()
                    {
                        for p in paths {
                            add_file_if_new(&mut self.files, p);
                        }
                    }
                }
                if ui.button("リストをクリア").clicked() {
                    self.files.clear();
                }
                ui.label(format!("選択中: {} 件", self.files.len()));
            });

            // ── ドロップエリア + ファイルリスト ──
            let frame = egui::Frame::group(ui.style()).fill(if hovering {
                egui::Color32::from_rgb(230, 245, 255)
            } else {
                ui.style().visuals.faint_bg_color
            });
            frame.show(ui, |ui| {
                ui.set_min_height(140.0);
                ui.set_width(ui.available_width());

                if self.files.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            egui::RichText::new("ここにファイルをドラッグ&ドロップ")
                                .size(14.0)
                                .weak(),
                        );
                        ui.label(egui::RichText::new("(複数ファイル可)").weak());
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let mut to_remove: Option<usize> = None;
                            for (i, f) in self.files.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.small_button("✕").on_hover_text("削除").clicked() {
                                        to_remove = Some(i);
                                    }
                                    ui.label(f.display().to_string());
                                });
                            }
                            if let Some(i) = to_remove {
                                self.files.remove(i);
                            }
                        });
                }
            });

            ui.add_space(6.0);
            ui.separator();

            // ── プリセット選択 ──
            // メーカーごとにグループ見出しを表示する(直前と同じグループなら省略)。
            ui.label(egui::RichText::new("変換プリセット").strong());
            ui.indent("preset", |ui| {
                let mut last_group: Option<&str> = None;
                for p in Preset::ALL {
                    if last_group != Some(p.group()) {
                        if last_group.is_some() {
                            ui.add_space(4.0);
                        }
                        ui.label(egui::RichText::new(p.group()).weak());
                        last_group = Some(p.group());
                    }
                    ui.radio_value(&mut self.preset, p, p.label());
                }
            });

            ui.separator();

            // ── 出力フォルダ ──
            ui.horizontal(|ui| {
                ui.label("出力フォルダ:");
                let dir_text = self
                    .output_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(入力ファイルと同じ場所)".to_string());
                ui.label(egui::RichText::new(dir_text).monospace());
                if ui.button("参照").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        self.output_dir = Some(d);
                    }
                }
                if self.output_dir.is_some() && ui.small_button("解除").clicked() {
                    self.output_dir = None;
                }
            });

            // ── ffmpeg パス ──
            ui.horizontal(|ui| {
                ui.label("ffmpeg:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.ffmpeg_path)
                        .desired_width(360.0)
                        .hint_text("ffmpeg.exe のパス、または PATH に通っていれば 'ffmpeg'"),
                );

                // 手動で編集された後に保存
                // (書き込み権限がないフォルダ(例: Program Files 配下)に
                // 配置されている場合など、保存に失敗することがある。
                // 黙って失敗すると「設定したのに再起動すると戻る」ように
                // 見えて分かりにくいため、失敗時はログに理由を残す)
                if resp.lost_focus() {
                    if let Err(e) = save_ffmpeg_path(&self.ffmpeg_path) {
                        push_log(
                            &self.log,
                            format!(
                                "ffmpeg パスの保存に失敗しました(次回起動時は反映されません): {}",
                                e
                            ),
                        );
                    }
                }

                // 参照ボタン: ffmpeg.exe を選択
                if ui
                    .button("参照…")
                    .on_hover_text("ffmpeg(.exe)ファイルを選択します")
                    .clicked()
                {
                    let mut dlg = rfd::FileDialog::new().set_title("ffmpeg 実行ファイルを選択");
                    if cfg!(windows) {
                        dlg = dlg.add_filter("実行ファイル", &["exe"]);
                    }
                    // 既に有効なパスがあればその親をデフォルトに
                    if let Some(parent) = std::path::Path::new(&self.ffmpeg_path)
                        .parent()
                        .filter(|p| p.exists() && !p.as_os_str().is_empty())
                    {
                        dlg = dlg.set_directory(parent);
                    }
                    if let Some(path) = dlg.pick_file() {
                        self.ffmpeg_path = path.to_string_lossy().into_owned();
                        push_log(&self.log, format!("ffmpeg を設定: {}", self.ffmpeg_path));
                        if let Err(e) = save_ffmpeg_path(&self.ffmpeg_path) {
                            push_log(
                                &self.log,
                                format!(
                                    "ffmpeg パスの保存に失敗しました(次回起動時は反映されません): {}",
                                    e
                                ),
                            );
                        }
                    }
                }

                // 動作確認 (ffmpeg -version)。
                // バックグラウンドスレッドで実行し、UIスレッドをブロックしない
                // (指定パスが応答しない実行ファイルだった場合に画面が固まるのを防ぐ)。
                if ui
                    .button("動作確認")
                    .on_hover_text("指定した ffmpeg が実際に起動できるか確認します")
                    .clicked()
                {
                    let path = self.ffmpeg_path.clone();
                    let log = self.log.clone();
                    let ctx = ctx.clone();
                    push_log(&log, format!("… ffmpeg を確認中: {}", path));
                    thread::spawn(move || {
                        if check_ffmpeg(&path) {
                            push_log(&log, format!("✓ ffmpeg OK: {}", path));
                        } else {
                            push_log(&log, format!("✗ ffmpeg を実行できません: {}", path));
                        }
                        ctx.request_repaint();
                    });
                }

                // 規定値(exe 横 or PATH)に戻す。
                // 保存済み設定は無視して自動検出し直す(initial_ffmpeg_path() を
                // 使うと、保存済みの値が有効な限りそれが読み戻されるだけで
                // 「自動検出に戻す」にならないため detect_default_ffmpeg_path() を使う)。
                if ui.button("既定").on_hover_text("自動検出に戻す").clicked() {
                    self.ffmpeg_path = detect_default_ffmpeg_path();
                    push_log(&self.log, format!("ffmpeg を既定値に戻しました: {}", self.ffmpeg_path));
                    if let Err(e) = save_ffmpeg_path(&self.ffmpeg_path) {
                        push_log(
                            &self.log,
                            format!(
                                "ffmpeg パスの保存に失敗しました(次回起動時は反映されません): {}",
                                e
                            ),
                        );
                    }
                }
            });

            // 状態表示 (ファイルとして存在するか / PATH 候補か)
            //
            // 空欄(または空白文字のみ)は「PATHで解決される裸のコマンド名」
            // ではなく「未設定」として別扱いする。空文字は is_bare_command()
            // が true を返す(区切り文字を含まないため)が、実際には
            // Command::new("") は必ず起動失敗するので、紛らわしい
            // 「PATHから解決します」という表示にはしない。
            ui.horizontal(|ui| {
                ui.add_space(56.0);
                if self.ffmpeg_path.trim().is_empty() {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 0, 0),
                        "● ffmpeg のパスが未設定です",
                    );
                } else if is_bare_command(&self.ffmpeg_path) {
                    ui.colored_label(
                        egui::Color32::from_rgb(140, 100, 0),
                        "● PATH から解決します(動作確認推奨)",
                    );
                } else if std::path::Path::new(&self.ffmpeg_path).exists() {
                    ui.colored_label(egui::Color32::from_rgb(0, 140, 0), "● ファイルを確認");
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 0, 0),
                        "● ファイルが見つかりません",
                    );
                }
            });

            ui.separator();

            // ── 実行ボタン + 進捗 ──
            let is_converting = *self.converting.lock_ignore_poison();
            let can_convert =
                !self.files.is_empty() && !is_converting && !self.ffmpeg_path.trim().is_empty();

            ui.horizontal(|ui| {
                ui.add_enabled_ui(can_convert, |ui| {
                    if ui
                        .add_sized(
                            [140.0, 32.0],
                            egui::Button::new(egui::RichText::new("変換実行").size(15.0)),
                        )
                        .clicked()
                    {
                        self.start_conversion();
                    }
                });

                if is_converting {
                    let (done, total) = *self.progress.lock_ignore_poison();
                    let frac = if total == 0 {
                        0.0
                    } else {
                        done as f32 / total as f32
                    };
                    ui.add(egui::ProgressBar::new(frac).text(format!("{}/{}", done, total)));
                    ctx.request_repaint();
                }
            });

            ui.separator();

            // ── ログ ──
            ui.label(egui::RichText::new("ログ").strong());
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .stick_to_bottom(true)
                .auto_shrink(false)
                .show(ui, |ui| {
                    for line in self.log.lock_ignore_poison().iter() {
                        ui.label(egui::RichText::new(line).monospace().size(12.0));
                    }
                });
        });

        // ── 録音時間オーバー / 長さ不明 警告ダイアログ ──
        let show = *self.show_skip_dialog.lock_ignore_poison();
        if show {
            // 背景を半透明で覆ってモーダル風にする
            let screen_rect = ctx.screen_rect();
            egui::Area::new(egui::Id::new("skip_modal_bg"))
                .order(egui::Order::Background)
                .fixed_pos(screen_rect.min)
                .show(ctx, |ui| {
                    ui.painter().rect_filled(
                        screen_rect,
                        0.0,
                        egui::Color32::from_black_alpha(128),
                    );
                });

            let mut close_clicked = false;

            egui::Window::new("変換をスキップしたファイルがあります")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 60, 0),
                        egui::RichText::new(
                            "⚠ 選択したプリセットの録音時間制限に適合しないファイルがあります",
                        )
                        .strong()
                        .size(14.0),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        "以下のファイルはスキップされました。あらかじめトリミングするか、\
                         別のプリセットを選んでから再度変換してください。",
                    );
                    ui.add_space(8.0);
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            for f in self.skipped_files.lock_ignore_poison().iter() {
                                ui.horizontal(|ui| {
                                    ui.label("•");
                                    ui.vertical(|ui| {
                                        ui.label(
                                            egui::RichText::new(f.path.display().to_string())
                                                .monospace(),
                                        );
                                        ui.label(
                                            egui::RichText::new(&f.reason)
                                                .size(11.0)
                                                .color(egui::Color32::from_rgb(140, 60, 0)),
                                        );
                                    });
                                });
                            }
                        });

                    ui.separator();
                    ui.add_space(4.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_sized([100.0, 28.0], egui::Button::new("閉じる"))
                            .clicked()
                        {
                            close_clicked = true;
                        }
                    });
                });

            if close_clicked {
                *self.show_skip_dialog.lock_ignore_poison() = false;
                self.skipped_files.lock_ignore_poison().clear();
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 日本語フォント設定 (Windows)
// ─────────────────────────────────────────────────────────────────────────────

fn setup_japanese_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\YuGothR.ttc",
        "C:\\Windows\\Fonts\\YuGothM.ttc",
        "C:\\Windows\\Fonts\\yugothic.ttc",
        "C:\\Windows\\Fonts\\msgothic.ttc",
        "C:\\Windows\\Fonts\\msmincho.ttc",
    ];

    let mut fonts = egui::FontDefinitions::default();
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts
                .font_data
                .insert("jp".to_owned(), egui::FontData::from_owned(data));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "jp".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "jp".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

// ─────────────────────────────────────────────────────────────────────────────
// main
//
// ウィンドウ/タスクバーアイコンは実行ファイルの Windows リソースとして
// build.rs が icon.ico を埋め込む方式に一本化している(Windows は
// 明示的な egui 側アイコン指定がなければ、実行ファイルのリソースアイコンを
// タイトルバー・タスクバーに使う)。かつてここには「icon.ico を読み込んで
// egui::IconData を作る」ための関数があったが、実際にはバイト列を読むだけで
// 中身を一切パースせず常に None を返す死んだコードだったため削除した。
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> Result<(), eframe::Error> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([760.0, 780.0])
        .with_min_inner_size([600.0, 600.0])
        .with_drag_and_drop(true)
        .with_title("音声ファイル変換ツール");

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "音声ファイル変換ツール",
        options,
        Box::new(|cc| {
            setup_japanese_fonts(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}

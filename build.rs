// build.rs — Windows ビルド時にプロジェクト直下の `icon.ico` を実行ファイルに埋め込む。
//
// - icon.ico が存在しない場合はスキップ(警告だけ出してビルドは続行)
// - Windows 以外のターゲットでは何もしない
// - icon.ico を変更した場合、cargo は自動的に再ビルドする
//
// 依存クレートについて:
// アイコン埋め込みには `winres` ではなく `winresource` を使う。
// `winres` は 2021 年を最後に更新が止まっており(Rust 1.61 以降では
// 動作しないケースがある)、事実上メンテナンスされていない。
// `winresource` は同じ API を維持したまま保守が続けられているフォークで、
// 依存の書き換えだけで済む(呼び出しコードは無変更)。
//
// ターゲット判定について:
// `#[cfg(windows)]` は「build.rs 自身をコンパイルしているホストOS」を見て
// しまうため、Linux/macOS から Windows 向けにクロスコンパイルする場合に
// 正しく動作しない。Cargo が設定する環境変数 `CARGO_CFG_TARGET_OS` は
// 「実際のビルド対象(ターゲット)OS」を指すので、そちらを見て判定する。

fn main() {
    // icon.ico が変更されたら再ビルド
    println!("cargo:rerun-if-changed=icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    if std::path::Path::new("icon.ico").exists() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icon.ico");
        // バージョン情報なども必要に応じて:
        // res.set("ProductName", "音声ファイル変換ツール");
        // res.set("FileDescription", "電話システム向け音声ファイル変換");
        if let Err(e) = res.compile() {
            println!("cargo:warning=アイコン埋め込みに失敗しました: {}", e);
        }
    } else {
        println!("cargo:warning=icon.ico が見つかりません。アイコンなしでビルドします。");
    }
}

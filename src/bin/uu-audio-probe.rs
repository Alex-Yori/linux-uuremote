//! 音频探针：抓默认输出 monitor N 秒，只报包数/码率/间隔（不碰内容）。
//! `UU_AUDIO_SOURCE` 可指定别的 source。

#[tokio::main]
async fn main() {
    let secs = std::env::args()
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5)
        .clamp(1, 60);
    match linux_uuremote::audio::probe(secs).await {
        Ok(()) => println!("音频探针 PASS"),
        Err(e) => {
            println!("音频探针 FAIL：{e}");
            std::process::exit(1);
        }
    }
}

//! Dedicated P1 probe binary: `cargo run --bin probe -- --index 0`.

fn main() {
    if let Err(error) = bluetooth_audio_receiver::probe::run() {
        eprintln!("错误: {error:#}");
        std::process::exit(1);
    }
}

// Bench: encode 2560x1440 @30fps with different thread counts.
use linux_uuremote::encoder::OpenH264;
use std::time::Instant;

fn main() {
    let (w, h) = (2560usize, 1440usize);
    let mut frame = vec![0u8; w * h * 4];
    for (i, px) in frame.iter_mut().enumerate() {
        *px = ((i / 4) % 251) as u8;
    }
    for threads in [4usize, 8, 12, 16] {
        let enc = OpenH264::new(w, h, 30, 4000, 60, threads).unwrap();
        let mut e = enc;
        // warm-up IDR
        e.encode_bgra(&frame, true).unwrap();
        let t0 = Instant::now();
        let n = 12;
        for i in 0..n {
            // churn pixels so the encoder does real P-frame work
            for (j, px) in frame.iter_mut().enumerate().take(w * 40 * 4) {
                *px = ((i * 7 + j / 4) % 253) as u8;
            }
            e.encode_bgra(&frame, false).unwrap();
        }
        let ms = t0.elapsed().as_millis() as f64 / n as f64;
        println!("threads={threads}: {ms:.1} ms/frame → {:.1} fps", 1000.0 / ms);
    }
}

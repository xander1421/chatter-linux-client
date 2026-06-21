//! Verify libyuv byte order for Slint's `Rgba8Pixel` (memory R, G, B, A).
//!
//! Picks Y/U/V values that yield three distinguishable colors and prints
//! the first 4 dst bytes for each conversion. If a converter outputs the
//! expected R, G, B, A byte order, the colors line up with the labels.

fn run(label: &str, y: u8, u: u8, v: u8) {
    let y_data = vec![y; 4];
    let u_data = vec![u; 1];
    let v_data = vec![v; 1];

    let mut rgba = vec![0u8; 16];
    livekit::webrtc::native::yuv_helper::i420_to_rgba(
        &y_data, 2, &u_data, 1, &v_data, 1,
        &mut rgba, 8, 2, 2,
    );

    let mut abgr = vec![0u8; 16];
    livekit::webrtc::native::yuv_helper::i420_to_abgr(
        &y_data, 2, &u_data, 1, &v_data, 1,
        &mut abgr, 8, 2, 2,
    );

    let mut argb = vec![0u8; 16];
    livekit::webrtc::native::yuv_helper::i420_to_argb(
        &y_data, 2, &u_data, 1, &v_data, 1,
        &mut argb, 8, 2, 2,
    );

    let mut bgra = vec![0u8; 16];
    livekit::webrtc::native::yuv_helper::i420_to_bgra(
        &y_data, 2, &u_data, 1, &v_data, 1,
        &mut bgra, 8, 2, 2,
    );

    println!("{label}: Y={y} U={u} V={v}");
    println!("  i420_to_rgba bytes = {:?}", &rgba[..4]);
    println!("  i420_to_abgr bytes = {:?}", &abgr[..4]);
    println!("  i420_to_argb bytes = {:?}", &argb[..4]);
    println!("  i420_to_bgra bytes = {:?}", &bgra[..4]);
    println!();
}

fn main() {
    // BT.601 limited-range YUV for full-saturation primaries.
    run("pure RED   (expect bytes R=255, G=0,   B=0,   A=255)", 81, 90, 240);
    run("pure GREEN (expect bytes R=0,   G=255, B=0,   A=255)", 145, 54, 34);
    run("pure BLUE  (expect bytes R=0,   G=0,   B=255, A=255)", 41, 240, 110);
}

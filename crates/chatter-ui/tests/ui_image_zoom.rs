//! Pure zoom math used by the full-screen viewer.

use i_slint_backend_testing as testing;

slint::slint! {
    import { ImageZoomMath } from "../../ui/components/image_viewer.slint";
    export component ZoomMathHarness inherits Window {
        in-out property <length> focal: ImageZoomMath.focal-offset(0px, 1.0, 2.5, 300px, 400px);
        in-out property <length> clamped: ImageZoomMath.clamp-offset(180px, 200px, 400px, 3.0);
        in-out property <length> fit-clamped: ImageZoomMath.clamp-offset(50px, 200px, 400px, 1.0);
        in-out property <float> double-tap-in: ImageZoomMath.double-tap-scale(1.0);
        in-out property <float> double-tap-out: ImageZoomMath.double-tap-scale(2.5);
    }}

#[test]
fn zoom_math_keeps_focal_point_and_clamps_edges() {
    testing::init_no_event_loop();
    let window = ZoomMathHarness::new().unwrap();
    assert!((window.get_focal() + 150.).abs() < 0.01);
    assert!((window.get_clamped() - 100.).abs() < 0.01);
    assert_eq!(window.get_fit_clamped(), 0.);
    assert_eq!(window.get_double_tap_in(), 2.5);
    assert_eq!(window.get_double_tap_out(), 1.0);
}

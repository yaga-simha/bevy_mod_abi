use super::*;

#[test]
fn latency_window_percentiles_and_resize_preserve_recent_samples() {
    let mut w = LatencyWindow::new(4);
    assert_eq!(w.compute_percentiles(), (0, 0));

    assert_eq!(w.record_and_percentiles(100), (100, 100));
    assert_eq!(w.record_and_percentiles(200), (100, 200));
    assert_eq!(w.record_and_percentiles(300), (200, 300));
    assert_eq!(w.record_and_percentiles(400), (200, 400));
    // Overwrites oldest (100) -> chronological [200, 300, 400, 500]
    assert_eq!(w.record_and_percentiles(500), (300, 500));

    // Shrink window from 4 -> 2: retains two most recent samples [400, 500].
    w.resize(2);
    assert_eq!(w.compute_percentiles(), (400, 500));

    // Grow window from 2 -> 5 and add 50 -> [400, 500, 50] sorted [50, 400, 500].
    w.resize(5);
    assert_eq!(w.record_and_percentiles(50), (400, 500));

    // Zero-clamped window size still works cleanly.
    w.resize(0);
    assert_eq!(w.compute_percentiles(), (50, 50));
}

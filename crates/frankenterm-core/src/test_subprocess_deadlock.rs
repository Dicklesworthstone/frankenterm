use std::time::{Duration, Instant};

#[test]
fn test_subprocess_deadlock() {
    let bridge = crate::subprocess_bridge::SubprocessBridge::<serde_json::Value>::new("sh")
        .with_timeout(Duration::from_millis(500));
    
    // sh -c "sleep 10 &" exits immediately but the background sleep holds stdout/stderr
    let start = Instant::now();
    let res = bridge.invoke(&["-c", "sleep 10 >&1 2>&2 &"]);
    
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(res.is_err());
}
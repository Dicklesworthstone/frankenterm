//! Backend-selection conformance matrix for `DispatchReactor`.
//!
//! wa-283h4.18: exhaustively exercise the cartesian product of
//! `(preference, stream_kind, io_uring_compiled, io_uring_runtime_available)`
//! against `DispatchReactor::resolve_with_availability`, in addition to the
//! coarse `DispatchIoBackend::current_default()` probe that the in-crate unit
//! tests already cover.

use frankenterm_mux_server_impl::dispatch::{
    DispatchIoBackend, DispatchIoPreference, DispatchIoRuntimeAvailability, DispatchReactor,
    DispatchRuntimeConfig, DispatchStreamKind,
};

fn readiness() -> DispatchIoBackend {
    DispatchIoBackend::readiness_default()
}

#[derive(Clone, Copy, Debug)]
enum ExpectedFallback {
    None,
    Some,
    ContainsUnix,
    ContainsUnavailable,
    ContainsCompiled,
}

fn check(
    pref: DispatchIoPreference,
    stream: DispatchStreamKind,
    compiled: bool,
    runtime: bool,
    want_backend: DispatchIoBackend,
    want_fallback: ExpectedFallback,
) {
    let reactor = DispatchReactor::resolve_with_availability(
        DispatchRuntimeConfig::new(pref),
        stream,
        DispatchIoRuntimeAvailability::for_conformance(compiled, runtime),
    );
    assert_eq!(
        reactor.backend(),
        want_backend,
        "backend mismatch for pref={pref:?} stream={stream:?} compiled={compiled} runtime={runtime}",
    );
    match want_fallback {
        ExpectedFallback::None => assert!(
            reactor.fallback_reason().is_none(),
            "expected no fallback_reason for pref={pref:?} stream={stream:?} compiled={compiled} runtime={runtime}, got {:?}",
            reactor.fallback_reason()
        ),
        ExpectedFallback::Some => assert!(
            reactor.fallback_reason().is_some(),
            "expected fallback_reason for pref={pref:?} stream={stream:?} compiled={compiled} runtime={runtime}",
        ),
        ExpectedFallback::ContainsUnix => {
            let reason = reactor
                .fallback_reason()
                .expect("fallback_reason should be populated");
            assert!(
                reason.contains("UnixStream"),
                "fallback_reason should mention UnixStream, got {reason}"
            );
        }
        ExpectedFallback::ContainsUnavailable => {
            let reason = reactor
                .fallback_reason()
                .expect("fallback_reason should be populated");
            assert!(
                reason.contains("unavailable"),
                "fallback_reason should mention runtime unavailable, got {reason}"
            );
        }
        ExpectedFallback::ContainsCompiled => {
            let reason = reactor
                .fallback_reason()
                .expect("fallback_reason should be populated");
            assert!(
                reason.contains("compiled"),
                "fallback_reason should mention compile-time flag, got {reason}"
            );
        }
    }
}

#[test]
fn auto_prefers_io_uring_only_for_unix_with_full_availability() {
    check(
        DispatchIoPreference::Auto,
        DispatchStreamKind::Unix,
        true,
        true,
        DispatchIoBackend::IoUring,
        ExpectedFallback::None,
    );
}

#[test]
fn auto_falls_back_for_tls_regardless_of_availability() {
    for (compiled, runtime) in [(true, true), (true, false), (false, true), (false, false)] {
        check(
            DispatchIoPreference::Auto,
            DispatchStreamKind::Tls,
            compiled,
            runtime,
            readiness(),
            ExpectedFallback::ContainsUnix,
        );
    }
}

#[test]
fn auto_falls_back_for_generic_regardless_of_availability() {
    for (compiled, runtime) in [(true, true), (true, false), (false, true), (false, false)] {
        check(
            DispatchIoPreference::Auto,
            DispatchStreamKind::Generic,
            compiled,
            runtime,
            readiness(),
            ExpectedFallback::ContainsUnix,
        );
    }
}

#[test]
fn auto_unix_without_compile_flag_falls_back_and_explains_why() {
    check(
        DispatchIoPreference::Auto,
        DispatchStreamKind::Unix,
        false,
        true,
        readiness(),
        ExpectedFallback::ContainsCompiled,
    );
}

#[test]
fn auto_unix_with_compile_flag_but_no_runtime_falls_back_and_explains_why() {
    check(
        DispatchIoPreference::Auto,
        DispatchStreamKind::Unix,
        true,
        false,
        readiness(),
        ExpectedFallback::ContainsUnavailable,
    );
}

#[test]
fn explicit_io_uring_for_tls_falls_back_with_unix_reason() {
    check(
        DispatchIoPreference::IoUring,
        DispatchStreamKind::Tls,
        true,
        true,
        readiness(),
        ExpectedFallback::ContainsUnix,
    );
}

#[test]
fn explicit_io_uring_unix_no_compile_falls_back() {
    check(
        DispatchIoPreference::IoUring,
        DispatchStreamKind::Unix,
        false,
        true,
        readiness(),
        ExpectedFallback::ContainsCompiled,
    );
}

#[test]
fn explicit_io_uring_unix_no_runtime_falls_back() {
    check(
        DispatchIoPreference::IoUring,
        DispatchStreamKind::Unix,
        true,
        false,
        readiness(),
        ExpectedFallback::ContainsUnavailable,
    );
}

#[test]
fn explicit_io_uring_unix_full_availability_selects_io_uring() {
    check(
        DispatchIoPreference::IoUring,
        DispatchStreamKind::Unix,
        true,
        true,
        DispatchIoBackend::IoUring,
        ExpectedFallback::None,
    );
}

#[test]
fn explicit_poll_always_selects_poll() {
    for stream in [
        DispatchStreamKind::Unix,
        DispatchStreamKind::Tls,
        DispatchStreamKind::Generic,
    ] {
        for (compiled, runtime) in [(true, true), (true, false), (false, true), (false, false)] {
            check(
                DispatchIoPreference::Poll,
                stream,
                compiled,
                runtime,
                DispatchIoBackend::Poll,
                ExpectedFallback::None,
            );
        }
    }
}

#[test]
fn explicit_epoll_is_linux_only() {
    let reactor = DispatchReactor::resolve_with_availability(
        DispatchRuntimeConfig::new(DispatchIoPreference::Epoll),
        DispatchStreamKind::Unix,
        DispatchIoRuntimeAvailability::for_conformance(false, false),
    );
    #[cfg(target_os = "linux")]
    {
        assert_eq!(reactor.backend(), DispatchIoBackend::Epoll);
        assert!(reactor.fallback_reason().is_none());
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(reactor.backend(), readiness());
        assert!(reactor.fallback_reason().is_some());
        let _ = ExpectedFallback::Some;
    }
}

#[test]
fn explicit_kqueue_is_bsd_and_darwin_only() {
    let reactor = DispatchReactor::resolve_with_availability(
        DispatchRuntimeConfig::new(DispatchIoPreference::Kqueue),
        DispatchStreamKind::Unix,
        DispatchIoRuntimeAvailability::for_conformance(false, false),
    );
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        assert_eq!(reactor.backend(), DispatchIoBackend::Kqueue);
        assert!(reactor.fallback_reason().is_none());
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        assert_eq!(reactor.backend(), readiness());
        assert!(reactor.fallback_reason().is_some());
    }
}

#[test]
fn readiness_default_matches_platform() {
    let readiness = DispatchIoBackend::readiness_default();
    #[cfg(target_os = "linux")]
    assert_eq!(readiness, DispatchIoBackend::Epoll);
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    assert_eq!(readiness, DispatchIoBackend::Kqueue);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    assert_eq!(readiness, DispatchIoBackend::Poll);
}

#[test]
fn current_default_reflects_feature_and_platform() {
    let def = DispatchIoBackend::current_default();
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    assert_eq!(def, DispatchIoBackend::IoUring);
    #[cfg(not(all(feature = "io-uring", target_os = "linux")))]
    assert_eq!(def, DispatchIoBackend::readiness_default());
}

// mac-gui-frame-meter: the terminal-agnostic FPS meter and main-thread
// responsiveness probe behind scripts/mac-gui-throughput.sh (ft-yccm0.1.4).
// The harness compiles it with `xcrun swiftc -O` into its cache directory.
//
//   frame-meter preflight [--font FAMILY]
//       Prints JSON: Screen Recording (TCC) and Accessibility trust for this
//       process's responsible app, every display's refresh rate and scale,
//       and whether FAMILY is installed. Never prompts.
//   frame-meter capture --pid PID --out FILE --stop-file PATH
//                       [--ready-file PATH] [--max-seconds N]
//                       [--title-contains TOKEN] [--title-wait-seconds N]
//                       [--pid-exclusive 1]
//                       [--window-wait-seconds N] [--max-width PX] [--fps-cap N]
//       Captures a window of PID with ScreenCaptureKit until PATH exists,
//       writing one JSON line per delivered frame: display time, arrival
//       time, SCFrameStatus, dirty-rect area fraction and a pixel hash. The
//       window is the largest on-screen one whose title contains TOKEN
//       (match "title"). With --pid-exclusive 1 (PID was started for this run
//       alone), an untitled window that is PID's only normal-level window
//       matches too (match "pid"): ScreenCaptureKit reports every Ghostty
//       window untitled (ft-5azl8). Only after --title-wait-seconds without
//       either does it fall back to the largest window of PID (match
//       "largest", title_verified false), which the harness refuses. A process
//       can briefly own other windows (Ghostty replaced its first one under
//       load, ft-zkjhg). When frames report the stream suspended or stopped,
//       the window is matched again and the stream follows it ("reacquire"
//       records). The ready file is written once frames are flowing. A
//       capture that cannot start writes an "error" record saying why.
//   frame-meter probe --pid PID --out FILE --stop-file PATH
//                     [--interval-ms N] [--timeout-ms N] [--hang-ms N]
//                     [--sample-dir DIR] [--max-samples N]
//       Asks PID's main thread for its window list through the Accessibility
//       API every N ms and writes each round trip's latency. A request still
//       outstanding after --hang-ms (default 2000, the beach-ball threshold)
//       triggers `/usr/bin/sample PID 2` into --sample-dir, at most
//       --max-samples times.
//
// Clocks: every timestamp is nanoseconds on CLOCK_UPTIME_RAW, the clock of
// mach_absolute_time. ScreenCaptureKit's displayTime is converted to it, and
// the harness's TTY-offset sampler reads the same clock.
//
// Exit codes: 0 done, 2 usage, 3 no window for PID, 4 permission denied,
// 5 capture failed.

import AppKit
import ApplicationServices
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit

// MARK: - Common

let timebase: mach_timebase_info_data_t = {
    var info = mach_timebase_info_data_t()
    mach_timebase_info(&info)
    return info
}()

func machToNs(_ ticks: UInt64) -> UInt64 {
    ticks / UInt64(timebase.denom) * UInt64(timebase.numer)
        + ticks % UInt64(timebase.denom) * UInt64(timebase.numer) / UInt64(timebase.denom)
}

func uptimeNs() -> UInt64 {
    clock_gettime_nsec_np(CLOCK_UPTIME_RAW)
}

func fail(_ code: Int32, _ message: String) -> Never {
    FileHandle.standardError.write(("frame-meter: " + message + "\n").data(using: .utf8)!)
    exit(code)
}

func jsonLine(_ object: [String: Any]) -> Data {
    var data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data()
    data.append(0x0A)
    return data
}

/// Serialized JSON-lines writer; every line is flushed to the file at once so
/// a killed meter still leaves its frames behind.
final class LineWriter {
    private let handle: FileHandle
    private let lock = NSLock()

    init(path: String) {
        guard FileManager.default.createFile(atPath: path, contents: nil),
            let handle = FileHandle(forWritingAtPath: path)
        else {
            fail(2, "cannot create \(path)")
        }
        self.handle = handle
    }

    func write(_ object: [String: Any]) {
        let data = jsonLine(object)
        lock.lock()
        handle.write(data)
        lock.unlock()
    }

    func close() {
        lock.lock()
        try? handle.synchronize()
        try? handle.close()
        lock.unlock()
    }
}

struct Arguments {
    var command = ""
    var values: [String: String] = [:]

    init(_ argv: [String]) {
        guard argv.count >= 2 else { return }
        command = argv[1]
        var index = 2
        while index < argv.count {
            let flag = argv[index]
            guard flag.hasPrefix("--"), index + 1 < argv.count else {
                fail(2, "expected --flag VALUE pairs, got \(flag)")
            }
            values[String(flag.dropFirst(2))] = argv[index + 1]
            index += 2
        }
    }

    func string(_ name: String) -> String? { values[name] }

    func required(_ name: String) -> String {
        guard let value = values[name] else { fail(2, "--\(name) is required") }
        return value
    }

    func int(_ name: String, _ fallback: Int) -> Int {
        guard let text = values[name] else { return fallback }
        guard let value = Int(text), value >= 0 else { fail(2, "--\(name) needs a whole number") }
        return value
    }
}

// MARK: - Displays

func displayRecords() -> [[String: Any]] {
    var records: [[String: Any]] = []
    for screen in NSScreen.screens {
        let number = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber
        let displayID = CGDirectDisplayID(number?.uint32Value ?? 0)
        var record: [String: Any] = [
            "display_id": displayID,
            "name": screen.localizedName,
            "max_fps": screen.maximumFramesPerSecond,
            "scale": screen.backingScaleFactor,
            "frame_points": [
                screen.frame.origin.x, screen.frame.origin.y, screen.frame.width, screen.frame.height,
            ],
            "main": displayID == CGMainDisplayID(),
        ]
        if let mode = CGDisplayCopyDisplayMode(displayID) {
            record["mode_refresh_hz"] = mode.refreshRate
            record["mode_points"] = [mode.width, mode.height]
            record["mode_pixels"] = [mode.pixelWidth, mode.pixelHeight]
        }
        records.append(record)
    }
    return records
}

func displayRecord(for displayID: CGDirectDisplayID) -> [String: Any]? {
    displayRecords().first { ($0["display_id"] as? CGDirectDisplayID) == displayID }
}

// MARK: - preflight

func preflight(_ args: Arguments) -> Never {
    var report: [String: Any] = [
        "screen_capture_access": CGPreflightScreenCaptureAccess(),
        "accessibility_trusted": AXIsProcessTrusted(),
        "displays": displayRecords(),
        "mach_timebase": [timebase.numer, timebase.denom],
        "uptime_ns": uptimeNs(),
    ]
    if let family = args.string("font") {
        report["font"] = [
            "family": family,
            "installed": NSFontManager.shared.availableFontFamilies.contains(family),
        ]
    }
    FileHandle.standardOutput.write(jsonLine(report))
    exit(0)
}

// MARK: - capture

/// FNV-1a over 64-bit words of every row's visible pixels.
func pixelHash(_ buffer: CVPixelBuffer) -> UInt64 {
    CVPixelBufferLockBaseAddress(buffer, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(buffer) else { return 0 }
    let width = CVPixelBufferGetWidth(buffer)
    let height = CVPixelBufferGetHeight(buffer)
    let bytesPerRow = CVPixelBufferGetBytesPerRow(buffer)
    let words = width * 4 / 8
    var hash: UInt64 = 0xcbf2_9ce4_8422_2325
    for row in 0..<height {
        let pointer = (base + row * bytesPerRow).assumingMemoryBound(to: UInt64.self)
        for index in 0..<words {
            hash = (hash ^ pointer[index]) &* 0x0000_0100_0000_01b3
        }
    }
    return hash
}

func rectArea(_ value: Any) -> Double {
    if let dictionary = value as? NSDictionary,
        let rect = CGRect(dictionaryRepresentation: dictionary as CFDictionary)
    {
        return Double(rect.width * rect.height)
    }
    if let boxed = value as? NSValue {
        let rect = boxed.rectValue
        return Double(rect.width * rect.height)
    }
    return 0
}

/// SCFrameStatus values that mean the captured window stopped producing
/// frames: suspended (3) and stopped (5).
let interruptedStatuses: Set<Int> = [
    SCFrameStatus.suspended.rawValue, SCFrameStatus.stopped.rawValue,
]

final class FrameRecorder: NSObject, SCStreamOutput {
    let writer: LineWriter
    let lock = NSLock()
    var statusCounts: [Int: Int] = [:]
    var frames = 0
    var lastCompleteNs: UInt64 = 0
    /// Called on the sample queue when a frame reports the stream suspended
    /// or stopped, with that frame's arrival time.
    var onInterruption: ((Int, UInt64) -> Void)?

    init(writer: LineWriter) {
        self.writer = writer
    }

    func lastComplete() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return lastCompleteNs
    }

    func stream(
        _ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of type: SCStreamOutputType
    ) {
        guard type == .screen else { return }
        let arrival = uptimeNs()
        guard
            let attachments = CMSampleBufferGetSampleAttachmentsArray(
                sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
            let info = attachments.first
        else { return }
        let status = (info[.status] as? NSNumber)?.intValue ?? -1
        let displayTicks = (info[.displayTime] as? NSNumber)?.uint64Value ?? 0
        var record: [String: Any] = [
            "type": "frame",
            "display_ns": displayTicks == 0 ? 0 : machToNs(displayTicks),
            "arrival_ns": arrival,
            "status": status,
        ]
        if status == SCFrameStatus.complete.rawValue,
            let buffer = CMSampleBufferGetImageBuffer(sampleBuffer)
        {
            let area = Double(CVPixelBufferGetWidth(buffer) * CVPixelBufferGetHeight(buffer))
            if let rects = info[.dirtyRects] as? [Any], area > 0 {
                let scale = (info[.scaleFactor] as? NSNumber)?.doubleValue ?? 1
                let dirty = rects.reduce(0.0) { $0 + rectArea($1) } * scale * scale
                record["dirty_fraction"] = min(1.0, dirty / area)
            }
            record["hash"] = String(pixelHash(buffer), radix: 16)
        }
        writer.write(record)
        lock.lock()
        statusCounts[status, default: 0] += 1
        frames += 1
        if status == SCFrameStatus.complete.rawValue {
            lastCompleteNs = arrival
        }
        let handler = onInterruption
        lock.unlock()
        if interruptedStatuses.contains(status) {
            handler?(status, arrival)
        }
    }
}

/// Writes an `error` record the harness reads back, then exits.
func failRecord(
    _ writer: LineWriter, _ code: Int32, _ reason: String, extra: [String: Any] = [:]
) -> Never {
    var record: [String: Any] = [
        "type": "error", "code": code, "reason": reason, "at_ns": uptimeNs(),
    ]
    record.merge(extra) { current, _ in current }
    writer.write(record)
    writer.close()
    fail(code, reason)
}

func describeWindow(_ window: SCWindow) -> [String: Any] {
    [
        "window_id": window.windowID,
        "title": window.title ?? "",
        "on_screen": window.isOnScreen,
        "layer": window.windowLayer,
        "frame_points": [
            window.frame.origin.x, window.frame.origin.y, window.frame.width, window.frame.height,
        ],
    ]
}

struct WindowChoice {
    let window: SCWindow
    /// How the window is known to be the run's: "title" (its title contains
    /// the run token), "pid" (PID's only normal-level window, PID being
    /// exclusive to the run) or "largest" (unverified).
    let match: String
    /// Every on-screen window of the process at the time of the choice.
    let candidates: [[String: Any]]

    /// The window's title contains the run token.
    var titleVerified: Bool { match == "title" }
}

/// The window to capture: the largest on-screen window of `pid` whose title
/// contains `token`; with `pidExclusive`, an untitled window that is the
/// only normal-level (layer 0) window of `pid`; only when `allowUnverified`,
/// the largest of all. A process can briefly own windows that are not the
/// terminal (Ghostty replaced its first window under load in nv3, ft-zkjhg,
/// and keeps a small panel open), so the title or the pid's sole normal
/// window is what proves the match. Two normal windows prove nothing.
func chooseWindow(
    _ content: SCShareableContent, pid: pid_t, token: String?, pidExclusive: Bool,
    allowUnverified: Bool
) -> WindowChoice? {
    let owned = content.windows.filter {
        $0.owningApplication?.processID == pid && $0.isOnScreen && $0.frame.width >= 100
            && $0.frame.height >= 100
    }
    let candidates = owned.map(describeWindow)
    let smaller: (SCWindow, SCWindow) -> Bool = {
        $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height
    }
    if let token = token, !token.isEmpty {
        if let match = owned.filter({ ($0.title ?? "").contains(token) }).max(by: smaller) {
            return WindowChoice(window: match, match: "title", candidates: candidates)
        }
    }
    let normal = owned.filter { $0.windowLayer == 0 }
    if pidExclusive, normal.count == 1, (normal[0].title ?? "").isEmpty {
        return WindowChoice(window: normal[0], match: "pid", candidates: candidates)
    }
    if let token = token, !token.isEmpty, !allowUnverified {
        return nil
    }
    guard let best = owned.max(by: smaller) else { return nil }
    return WindowChoice(window: best, match: "largest", candidates: candidates)
}

func shareableContent(_ completion: @escaping (SCShareableContent?, Error?) -> Void) {
    SCShareableContent.getExcludingDesktopWindows(
        false, onScreenWindowsOnly: true, completionHandler: completion)
}

/// Polls until a window qualifies: one titled with `token` (or, with
/// `pidExclusive`, `pid`'s only normal-level window) until `titleDeadline`,
/// any window of `pid` after it, nothing after `deadline`.
func findWindow(
    pid: pid_t, token: String?, pidExclusive: Bool, titleDeadline: Date, deadline: Date,
    writer: LineWriter, completion: @escaping (WindowChoice, SCShareableContent) -> Void
) {
    shareableContent { content, error in
        if let error = error, !CGPreflightScreenCaptureAccess() {
            failRecord(writer, 4, "Screen Recording permission denied (\(error))")
        }
        let allowUnverified = Date() > titleDeadline
        if let content = content,
            let choice = chooseWindow(
                content, pid: pid, token: token, pidExclusive: pidExclusive,
                allowUnverified: allowUnverified)
        {
            completion(choice, content)
            return
        }
        if Date() > deadline {
            let seen =
                content.map { listed in
                    listed.windows.filter { $0.owningApplication?.processID == pid }.map(
                        describeWindow)
                } ?? []
            var titled = token.map { " titled with \"\($0)\"" } ?? ""
            if pidExclusive {
                titled += token == nil ? " (as its only normal window)" : " or as its only normal window"
            }
            failRecord(
                writer, 3, "no on-screen window of pid \(pid)\(titled) appeared",
                extra: [
                    "candidates": seen,
                    "listing_error": error.map { String(describing: $0) } ?? "",
                ])
        }
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.25) {
            findWindow(
                pid: pid, token: token, pidExclusive: pidExclusive, titleDeadline: titleDeadline,
                deadline: deadline, writer: writer, completion: completion)
        }
    }
}

/// One capture: the stream, the window it follows, and re-acquisition when
/// the window stops producing frames (suspended, stopped, or replaced).
final class CaptureSession: NSObject, SCStreamDelegate {
    static let maxAttempts = 40

    let pid: pid_t
    let token: String?
    let pidExclusive: Bool
    let writer: LineWriter
    let recorder: FrameRecorder
    let configuration: SCStreamConfiguration
    let sampleQueue = DispatchQueue(label: "frame-meter.samples")
    let control = DispatchQueue(label: "frame-meter.control")
    // Guarded by `control`.
    var stream: SCStream?
    var windowID: CGWindowID = 0
    var match = "largest"
    var reacquiring = false
    var attempts = 0
    var stopError: String?

    init(
        pid: pid_t, token: String?, pidExclusive: Bool, writer: LineWriter,
        recorder: FrameRecorder, configuration: SCStreamConfiguration
    ) {
        self.pid = pid
        self.token = token
        self.pidExclusive = pidExclusive
        self.writer = writer
        self.recorder = recorder
        self.configuration = configuration
        super.init()
        recorder.onInterruption = { [weak self] status, at in
            self?.requestReacquire(reason: "frame status \(status)", since: at)
        }
    }

    /// Starts a stream on `choice`; `then` receives the start error, if any.
    func start(_ choice: WindowChoice, then: @escaping (Error?) -> Void) {
        let filter = SCContentFilter(desktopIndependentWindow: choice.window)
        let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
        do {
            try stream.addStreamOutput(recorder, type: .screen, sampleHandlerQueue: sampleQueue)
        } catch {
            then(error)
            return
        }
        control.sync {
            self.stream = stream
            self.windowID = choice.window.windowID
            self.match = choice.match
        }
        stream.startCapture(completionHandler: then)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        let text = String(describing: error)
        control.sync {
            self.stopError = text
            self.stream = nil
        }
        writer.write(["type": "stream_stopped", "at_ns": uptimeNs(), "error": text])
        requestReacquire(reason: "stream stopped: \(text)", since: uptimeNs())
    }

    func requestReacquire(reason: String, since: UInt64, delay: Double = 0.25) {
        control.async {
            guard !self.reacquiring, self.attempts < CaptureSession.maxAttempts else { return }
            self.reacquiring = true
            self.attempts += 1
            let attempt = self.attempts
            DispatchQueue.global().asyncAfter(deadline: .now() + delay) {
                self.reacquire(reason: reason, since: since, attempt: attempt)
            }
        }
    }

    private func finish(_ record: [String: Any], retry: (reason: String, since: UInt64)?) {
        writer.write(record)
        control.sync { self.reacquiring = false }
        if let retry = retry {
            requestReacquire(reason: retry.reason, since: retry.since, delay: 1.0)
        }
    }

    private func reacquire(reason: String, since: UInt64, attempt: Int) {
        shareableContent { content, error in
            let (currentID, verified, stream) = self.control.sync {
                (self.windowID, self.match != "largest", self.stream)
            }
            var record: [String: Any] = [
                "type": "reacquire", "reason": reason, "attempt": attempt, "at_ns": uptimeNs(),
                "since_ns": since,
            ]
            guard let content = content,
                let choice = chooseWindow(
                    content, pid: self.pid, token: self.token, pidExclusive: self.pidExclusive,
                    allowUnverified: !verified)
            else {
                record["outcome"] = "no matching window"
                if let error = error {
                    record["error"] = String(describing: error)
                }
                self.finish(record, retry: (reason, since))
                return
            }
            record["window_id"] = choice.window.windowID
            record["title"] = choice.window.title ?? ""
            record["title_verified"] = choice.titleVerified
            record["match"] = choice.match
            record["candidates"] = choice.candidates
            if stream != nil, choice.window.windowID == currentID {
                // The same window: ScreenCaptureKit resumes it on its own once
                // it is visible again. Look again later unless it has resumed.
                let resumed = self.recorder.lastComplete() > since
                record["outcome"] = resumed ? "same window, resumed" : "same window, waiting"
                self.finish(record, retry: resumed ? nil : (reason, since))
                return
            }
            if let stream = stream {
                let filter = SCContentFilter(desktopIndependentWindow: choice.window)
                stream.updateContentFilter(filter) { error in
                    record["outcome"] = error == nil ? "switched window" : "switch failed"
                    if let error = error {
                        record["error"] = String(describing: error)
                    } else {
                        self.control.sync {
                            self.windowID = choice.window.windowID
                            self.match = choice.match
                        }
                    }
                    self.finish(record, retry: error == nil ? nil : (reason, since))
                }
            } else {
                self.start(choice) { error in
                    record["outcome"] = error == nil ? "restarted stream" : "restart failed"
                    if let error = error {
                        record["error"] = String(describing: error)
                    }
                    self.finish(record, retry: error == nil ? nil : (reason, since))
                }
            }
        }
    }

    func summaryFields() -> [String: Any] {
        control.sync {
            [
                "reacquire_attempts": attempts,
                "final_window_id": windowID,
                "final_title_verified": match == "title",
                "final_match": match,
                "stream_error": stopError ?? "",
            ]
        }
    }
}

func capture(_ args: Arguments) -> Never {
    guard let pid = pid_t(args.required("pid")) else { fail(2, "--pid needs a process id") }
    let writer = LineWriter(path: args.required("out"))
    let stopFile = args.required("stop-file")
    let readyFile = args.string("ready-file")
    let maxSeconds = args.int("max-seconds", 3600)
    let maxWidth = max(160, args.int("max-width", 960))
    let fpsCap = max(1, args.int("fps-cap", 240))
    let token = args.string("title-contains")
    let pidExclusive = args.int("pid-exclusive", 0) != 0
    let waitSeconds = args.int("window-wait-seconds", 60)
    let deadline = Date().addingTimeInterval(TimeInterval(waitSeconds))
    let titleDeadline = Date().addingTimeInterval(
        TimeInterval(min(waitSeconds, args.int("title-wait-seconds", waitSeconds))))
    guard CGPreflightScreenCaptureAccess() else {
        failRecord(
            writer, 4,
            "Screen Recording permission is not granted to this process's responsible app")
    }

    findWindow(
        pid: pid, token: token, pidExclusive: pidExclusive, titleDeadline: titleDeadline,
        deadline: deadline, writer: writer
    ) { choice, content in
        let window = choice.window
        let center = CGPoint(x: window.frame.midX, y: window.frame.midY)
        let display = content.displays.first { $0.frame.contains(center) } ?? content.displays.first
        let displayInfo = display.flatMap { displayRecord(for: $0.displayID) } ?? [:]
        let scale = (displayInfo["scale"] as? CGFloat) ?? 2
        let pixelWidth = window.frame.width * scale
        let pixelHeight = window.frame.height * scale
        let shrink = min(1, CGFloat(maxWidth) / pixelWidth)

        let configuration = SCStreamConfiguration()
        configuration.width = max(2, Int(pixelWidth * shrink))
        configuration.height = max(2, Int(pixelHeight * shrink))
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fpsCap))
        configuration.queueDepth = 8
        configuration.showsCursor = false
        configuration.capturesAudio = false

        let header: [String: Any] = [
            "type": "window",
            "pid": pid,
            "window_id": window.windowID,
            "title": window.title ?? "",
            "app": window.owningApplication?.applicationName ?? "",
            "bundle_id": window.owningApplication?.bundleIdentifier ?? "",
            "frame_points": [
                window.frame.origin.x, window.frame.origin.y, window.frame.width, window.frame.height,
            ],
            "window_pixels": [Int(pixelWidth), Int(pixelHeight)],
            "capture_pixels": [configuration.width, configuration.height],
            "fps_cap": fpsCap,
            "display": displayInfo,
            "token": token ?? "",
            "title_verified": choice.titleVerified,
            "match": choice.match,
            "pid_exclusive": pidExclusive,
            "candidates": choice.candidates,
            "started_ns": uptimeNs(),
        ]
        writer.write(header)

        let recorder = FrameRecorder(writer: writer)
        let session = CaptureSession(
            pid: pid, token: token, pidExclusive: pidExclusive, writer: writer,
            recorder: recorder, configuration: configuration)
        captureSession = session
        session.start(choice) { error in
            if let error = error {
                if !CGPreflightScreenCaptureAccess() {
                    failRecord(writer, 4, "Screen Recording permission denied (\(error))")
                }
                failRecord(writer, 5, "startCapture failed: \(error)")
            }
            if let readyFile = readyFile {
                FileManager.default.createFile(atPath: readyFile, contents: jsonLine(header))
            }
        }
        let startedAt = Date()
        let timer = DispatchSource.makeTimerSource(queue: DispatchQueue.global())
        timer.schedule(deadline: .now() + 0.05, repeating: 0.05)
        timer.setEventHandler {
            let expired = Date().timeIntervalSince(startedAt) > TimeInterval(maxSeconds)
            guard FileManager.default.fileExists(atPath: stopFile) || expired else { return }
            timer.cancel()
            let writeSummary: (Error?) -> Void = { error in
                recorder.lock.lock()
                var counts: [String: Int] = [:]
                for (status, count) in recorder.statusCounts { counts[String(status)] = count }
                var summary: [String: Any] = [
                    "type": "summary",
                    "stopped_ns": uptimeNs(),
                    "frames": recorder.frames,
                    "status_counts": counts,
                    "stop_reason": expired ? "max-seconds" : "stop-file",
                    "stop_error": error.map { String(describing: $0) } ?? "",
                ]
                recorder.lock.unlock()
                summary.merge(session.summaryFields()) { current, _ in current }
                writer.write(summary)
                writer.close()
                exit(0)
            }
            if let stream = session.control.sync(execute: { session.stream }) {
                stream.stopCapture(completionHandler: writeSummary)
            } else {
                writeSummary(nil)
            }
        }
        timer.resume()
    }
    dispatchMain()
}

/// Keeps the capture session alive for the life of the process.
var captureSession: CaptureSession?

// MARK: - probe

final class HangWatch {
    let lock = NSLock()
    var inflightSince: UInt64 = 0
    var sampledThisHang = false
    var samplesTaken = 0
}

func probe(_ args: Arguments) -> Never {
    guard let pid = pid_t(args.required("pid")) else { fail(2, "--pid needs a process id") }
    let writer = LineWriter(path: args.required("out"))
    let stopFile = args.required("stop-file")
    let intervalNs = UInt64(max(10, args.int("interval-ms", 100))) * 1_000_000
    let timeoutMs = max(100, args.int("timeout-ms", 5000))
    let hangNs = UInt64(max(100, args.int("hang-ms", 2000))) * 1_000_000
    let sampleDir = args.string("sample-dir")
    let maxSamples = args.int("max-samples", 3)
    guard AXIsProcessTrusted() else {
        fail(4, "Accessibility permission is not granted to this process's responsible app")
    }

    let application = AXUIElementCreateApplication(pid)
    AXUIElementSetMessagingTimeout(application, Float(timeoutMs) / 1000)
    writer.write([
        "type": "probe", "pid": pid, "interval_ns": intervalNs, "timeout_ms": timeoutMs,
        "hang_ns": hangNs, "started_ns": uptimeNs(),
    ])

    let watch = HangWatch()
    let watchdog = DispatchSource.makeTimerSource(queue: DispatchQueue.global())
    watchdog.schedule(deadline: .now() + 0.1, repeating: 0.1)
    watchdog.setEventHandler {
        watch.lock.lock()
        let since = watch.inflightSince
        let due = since != 0 && uptimeNs() - since > hangNs && !watch.sampledThisHang
            && watch.samplesTaken < maxSamples && sampleDir != nil
        if due {
            watch.sampledThisHang = true
            watch.samplesTaken += 1
        }
        let index = watch.samplesTaken
        watch.lock.unlock()
        guard due, let sampleDir = sampleDir else { return }
        let path = "\(sampleDir)/hang-\(index)-pid\(pid).txt"
        let sampler = Process()
        sampler.executableURL = URL(fileURLWithPath: "/usr/bin/sample")
        sampler.arguments = [String(pid), "2", "-file", path]
        sampler.standardOutput = FileHandle.nullDevice
        sampler.standardError = FileHandle.nullDevice
        do {
            try sampler.run()
            writer.write(["type": "hang_sample", "at_ns": uptimeNs(), "path": path])
        } catch {
            writer.write(["type": "hang_sample", "at_ns": uptimeNs(), "error": "\(error)"])
        }
    }
    watchdog.resume()

    Thread.detachNewThread {
        var probes = 0
        while !FileManager.default.fileExists(atPath: stopFile) {
            let started = uptimeNs()
            watch.lock.lock()
            watch.inflightSince = started
            watch.sampledThisHang = false
            watch.lock.unlock()
            var value: CFTypeRef?
            let result = AXUIElementCopyAttributeValue(
                application, kAXWindowsAttribute as CFString, &value)
            let finished = uptimeNs()
            watch.lock.lock()
            watch.inflightSince = 0
            watch.lock.unlock()
            writer.write([
                "type": "ping", "start_ns": started, "latency_ns": finished - started,
                "ax_error": result.rawValue,
            ])
            probes += 1
            let elapsed = finished - started
            if elapsed < intervalNs {
                usleep(useconds_t((intervalNs - elapsed) / 1000))
            }
        }
        writer.write(["type": "summary", "stopped_ns": uptimeNs(), "probes": probes])
        writer.close()
        exit(0)
    }
    dispatchMain()
}

// MARK: - main

let arguments = Arguments(CommandLine.arguments)
switch arguments.command {
case "preflight": preflight(arguments)
case "capture": capture(arguments)
case "probe": probe(arguments)
default:
    fail(2, "usage: frame-meter preflight|capture|probe [--flag value ...]")
}

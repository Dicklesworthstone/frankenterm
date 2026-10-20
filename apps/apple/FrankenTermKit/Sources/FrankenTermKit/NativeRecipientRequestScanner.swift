#if os(macOS) && canImport(AVFoundation) && canImport(AppKit) && canImport(Vision)
import Foundation
import AppKit
import CoreMedia
import CoreVideo
@preconcurrency import AVFoundation
import Vision

public enum RecipientRequestCameraError: Error, Sendable, LocalizedError {
    case permissionDenied, unavailable, invalidRequest, multipleRequests, interrupted, timedOut
    public var errorDescription: String? {
        switch self {
        case .permissionDenied:
            "Allow camera access for FrankenTerm Offline Export in System Settings, then start a new scan."
        case .unavailable:
            "A compatible camera is unavailable. Connect a camera and start a new scan."
        case .invalidRequest:
            "The scanned recipient request is invalid or unsupported. Ask the phone to create a new request."
        case .multipleRequests:
            "More than one recipient request is visible. Show only the intended phone’s request and start a new scan."
        case .interrupted:
            "Recipient scanning stopped when the camera, app, or device became inactive. Start a new scan when ready."
        case .timedOut:
            "No recipient request was accepted within two minutes. Show the phone's request clearly and start a new scan."
        }
    }
}

/// An explicit, single-request camera surface. It recognizes only FTREQ1 and
/// emits one fully parsed public request; it never opens URLs, interprets terminal
/// commands, accepts workspace transfer frames, records audio, or saves images.
/// Construct this controller only after the user chooses Scan Phone Request.
@MainActor
public final class NativeRecipientRequestScanner: NSViewController {
    private let onRequest: @MainActor @Sendable (RecipientRequest) -> Void
    private let onFailure: @MainActor @Sendable (RecipientRequestCameraError) -> Void
    private var capture: RecipientRequestCapture?
    private var preview: AVCaptureVideoPreviewLayer?
    private var generation: UUID?
    private var retired = false
    private var permission: Task<Void, Never>?
    private let observations = RecipientCameraObservations()

    public init(onRequest: @escaping @MainActor @Sendable (RecipientRequest) -> Void,
                onFailure: @escaping @MainActor @Sendable (RecipientRequestCameraError) -> Void) {
        self.onRequest = onRequest; self.onFailure = onFailure
        super.init(nibName: nil, bundle: nil)
    }
    @available(*, unavailable)
    public required init?(coder: NSCoder) { return nil }

    deinit {
        // These Sendable owners can be retired even if AppKit tears down the
        // controller without issuing its normal disappearance callback.
        permission?.cancel(); capture?.stop()
    }

    public override func loadView() {
        let surface = NSView(frame: NSRect(x: 0, y: 0, width: 560, height: 340))
        surface.wantsLayer = true; surface.layer?.backgroundColor = NSColor.black.cgColor
        surface.setAccessibilityLabel("Recipient request camera")
        view = surface
        let capture = RecipientRequestCapture { [weak self] token, event in
            Task { @MainActor [weak self] in
                guard let self, self.generation == token else { return }
                guard NSApplication.shared.isActive, self.view.window != nil else {
                    self.stop(); self.onFailure(.interrupted); return
                }
                self.stop()
                switch event {
                case let .request(request): self.onRequest(request)
                case let .failure(error): self.onFailure(error)
                }
            }
        }
        self.capture = capture
        // Preview attachment happens once, before session configuration starts.
        // Capture configuration and all start/stop calls belong to its worker.
        let preview = AVCaptureVideoPreviewLayer(session: capture.session)
        preview.videoGravity = .resizeAspect
        preview.frame = surface.bounds
        surface.layer?.addSublayer(preview)
        self.preview = preview
    }

    public override func viewDidLayout() {
        super.viewDidLayout(); preview?.frame = view.bounds
    }

    public override func viewDidAppear() {
        super.viewDidAppear()
        guard generation == nil, !retired else { return }
        let token = UUID(); generation = token; preview?.isHidden = false
        observations.add(center: .default, name: NSApplication.didResignActiveNotification) { [weak self] in
            Task { @MainActor [weak self] in self?.interrupt(token: token) }
        }
        observations.add(center: NSWorkspace.shared.notificationCenter, name: NSWorkspace.sessionDidResignActiveNotification) { [weak self] in
            Task { @MainActor [weak self] in self?.interrupt(token: token) }
        }
        permission = Task { [weak self] in
            let authorized: Bool
            switch AVCaptureDevice.authorizationStatus(for: .video) {
            case .authorized: authorized = true
            case .notDetermined: authorized = await AVCaptureDevice.requestAccess(for: .video)
            default: authorized = false
            }
            guard !Task.isCancelled, let self, self.generation == token else { return }
            guard self.view.window != nil, NSApplication.shared.isActive else {
                self.stop(); self.onFailure(.interrupted); return
            }
            if authorized { self.capture?.start(token: token) }
            else { self.stop(); self.onFailure(.permissionDenied) }
        }
    }

    public override func viewWillDisappear() { stop(); super.viewWillDisappear() }

    /// Retire foreground delivery immediately; session shutdown follows on its
    /// serial worker. A late authorization or camera callback cannot revive it.
    public func stop() {
        retired = true; generation = nil; permission?.cancel(); permission = nil
        preview?.isHidden = true; observations.removeAll(); capture?.stop()
    }

    private func interrupt(token: UUID) {
        guard generation == token else { return }
        stop(); onFailure(.interrupted)
    }
}

/// NotificationCenter removal is thread-safe. This owner ensures registrations
/// are removed even if AppKit releases a view without another lifecycle callback.
private final class RecipientCameraObservations: @unchecked Sendable {
    private let lock = NSLock()
    private var entries: [(NotificationCenter, NSObjectProtocol)] = []
    func add(center: NotificationCenter, name: Notification.Name, object: AnyObject? = nil,
             action: @escaping @Sendable () -> Void) {
        let token = center.addObserver(forName: name, object: object, queue: nil) { _ in action() }
        lock.lock(); entries.append((center, token)); lock.unlock()
    }
    func removeAll() {
        lock.lock(); let old = entries; entries.removeAll(); lock.unlock()
        for (center, token) in old { center.removeObserver(token) }
    }
    deinit { removeAll() }
}

/// The AVFoundation objects and mutable state below are confined to `queue`.
/// The sole shared reference is the session's documented preview attachment,
/// created before start. Only immutable parsed requests cross to MainActor.
private final class RecipientRequestCapture: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate, @unchecked Sendable {
    enum Event: Sendable { case request(RecipientRequest), failure(RecipientRequestCameraError) }
    let session = AVCaptureSession()
    private let queue = DispatchQueue(label: "org.frankenterm.recipient-camera", qos: .userInitiated,
                                      autoreleaseFrequency: .workItem)
    private let completion: @Sendable (UUID, Event) -> Void
    private let observations = RecipientCameraObservations()
    private var generation: UUID?
    private var lastAttempt: UInt64 = 0

    init(completion: @escaping @Sendable (UUID, Event) -> Void) {
        self.completion = completion; super.init()
    }

    func start(token: UUID) {
        queue.async { [self] in
            guard generation == nil else { return }
            generation = token; lastAttempt = 0
            do { try configure() }
            catch { finish(.failure(.unavailable), token: token); return }
            for name in [AVCaptureSession.runtimeErrorNotification, AVCaptureSession.wasInterruptedNotification,
                         AVCaptureSession.didStopRunningNotification] {
                observations.add(center: .default, name: name, object: session) { [weak self] in
                    self?.failOnWorker(token: token)
                }
            }
            session.startRunning()
            guard session.isRunning else { finish(.failure(.unavailable), token: token); return }
            queue.asyncAfter(deadline: .now() + .seconds(120)) { [weak self] in
                self?.finish(.failure(.timedOut), token: token)
            }
        }
    }

    func stop() {
        queue.async { [self] in generation = nil; stopOnWorker() }
    }

    private func configure() throws {
        session.beginConfiguration()
        defer { session.commitConfiguration() }
        guard let camera = AVCaptureDevice.default(for: .video) else {
            throw RecipientRequestCameraError.unavailable
        }
        let input = try AVCaptureDeviceInput(device: camera)
        guard session.canAddInput(input) else { throw RecipientRequestCameraError.unavailable }
        session.addInput(input)
        let preset: AVCaptureSession.Preset = session.canSetSessionPreset(.hd1280x720) ? .hd1280x720 : .vga640x480
        guard session.canSetSessionPreset(preset) else { throw RecipientRequestCameraError.unavailable }
        session.sessionPreset = preset
        // Webcam metadata types are device-dependent. Decode ordinary video
        // buffers with Vision instead of requiring native QR metadata support.
        let video = AVCaptureVideoDataOutput()
        guard session.canAddOutput(video) else { throw RecipientRequestCameraError.unavailable }
        session.addOutput(video)
        guard video.availableVideoPixelFormatTypes.contains(kCVPixelFormatType_32BGRA) else {
            throw RecipientRequestCameraError.unavailable
        }
        video.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
        video.alwaysDiscardsLateVideoFrames = true
        video.setSampleBufferDelegate(self, queue: queue)
    }

    private func failOnWorker(token: UUID) {
        queue.async { [weak self] in self?.finish(.failure(.interrupted), token: token) }
    }

    private func stopOnWorker() {
        observations.removeAll()
        if session.isRunning { session.stopRunning() }
        session.beginConfiguration()
        for output in session.outputs {
            (output as? AVCaptureVideoDataOutput)?.setSampleBufferDelegate(nil, queue: nil)
            session.removeOutput(output)
        }
        for input in session.inputs { session.removeInput(input) }
        session.commitConfiguration()
    }

    private func finish(_ event: Event, token: UUID) {
        guard generation == token else { return }
        generation = nil
        completion(token, event)
        // Return from a video delegate callback before stopping its session.
        // There is only one terminal delivery, so no unbounded MainActor queue.
        queue.async { [self] in stopOnWorker() }
    }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer,
                       from connection: AVCaptureConnection) {
        guard let token = generation else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        guard lastAttempt == 0 || now - lastAttempt >= 250_000_000 else { return }
        lastAttempt = now
        guard let buffer = CMSampleBufferGetImageBuffer(sampleBuffer) else {
            finish(.failure(.unavailable), token: token); return
        }
        let width = CVPixelBufferGetWidth(buffer), height = CVPixelBufferGetHeight(buffer)
        guard width > 0, height > 0, max(width, height) <= 1280, min(width, height) <= 720,
              CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_32BGRA,
              CVPixelBufferGetDataSize(buffer) <= 4 * 1024 * 1024 else {
            finish(.failure(.unavailable), token: token); return
        }
        // Synchronous Vision processing retains only this bounded camera buffer
        // on the one worker. Late frames are discarded, not queued into Tasks.
        // Inspect at most eight QR observations and parse one request per frame.
        // Ambiguous destinations are rejected instead of choosing the first.
        let request = VNDetectBarcodesRequest()
        request.symbologies = [.qr]
        do { try VNImageRequestHandler(cvPixelBuffer: buffer, orientation: .up, options: [:]).perform([request]) }
        catch { finish(.failure(.unavailable), token: token); return }
        let observations = request.results ?? []
        guard observations.count <= 8 else { finish(.failure(.multipleRequests), token: token); return }
        let candidates = observations.compactMap { observation -> String? in
            guard observation.symbology == .qr, let text = observation.payloadStringValue,
                  text.hasPrefix("FTREQ1:") else { return nil }
            return text
        }
        guard !candidates.isEmpty else { return }
        guard candidates.count == 1 else { finish(.failure(.multipleRequests), token: token); return }
        let text = candidates[0]
        guard text.utf8.count <= RecipientRequest.maximumTextBytes else { finish(.failure(.invalidRequest), token: token); return }
        do { finish(.request(try RecipientRequest(text: text)), token: token) }
        catch { finish(.failure(.invalidRequest), token: token) }
    }
}
#endif

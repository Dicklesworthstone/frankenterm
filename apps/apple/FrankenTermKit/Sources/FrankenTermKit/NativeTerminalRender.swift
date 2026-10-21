#if canImport(UIKit) && canImport(MetalKit) && canImport(CoreText)
import UIKit
@preconcurrency import MetalKit
import CoreText

public struct NativeTerminalRenderConfiguration: Equatable, Sendable {
    public var fontName = "Menlo"
    /// PostScript names tried, in order, for characters the base font lacks,
    /// before the system cascade. "SymbolsNFM" is the bundled Symbols Nerd Font
    /// Mono (powerline separators and prompt icons, which Menlo lacks). Names
    /// that are not installed are skipped.
    public var fallbackFontNames = ["SymbolsNFM"]
    public var fontPointSize: Double = 15
    public var fontRevision: UInt64 = 1
    public var boldBrightensANSI = false
    public var cursorBlink = true
    public init() {}
    fileprivate func validate() throws {
        guard fontRevision > 0, !fontName.isEmpty, fontName.utf8.count <= 256,
              fallbackFontNames.count <= 8, fallbackFontNames.allSatisfy({ !$0.isEmpty && $0.utf8.count <= 256 }),
              fontPointSize.isFinite, (6...48).contains(fontPointSize)
        else { throw TerminalRenderError.capacity }
    }
}

/// A local viewport proposal. The session owner must apply this to the canonical
/// terminal and SSH PTY, then return a new semantic frame; this does not resize a
/// remote terminal merely because a UIKit view changed size.
public struct NativeTerminalViewport: Equatable, Sendable {
    public let source: TerminalSource
    public let revision: UInt64
    public let columns: Int, rows: Int, pixelWidth: Int, pixelHeight: Int
}

/// One native scene surface. Admission, source identity and UIKit stay on
/// MainActor; Core Text, rasterization, atlas writes and buffer preparation stay
/// on one dedicated serial worker. The three GPU slots are never written while
/// submitted, including after cancellation, retirement, or scene deactivation.
@MainActor
public final class NativeTerminalRenderView: MTKView {
    public let source: TerminalSource
    public var onCaret: (@MainActor (NativeTerminalCaret) -> Void)?
    public var onCaretUnavailable: (@MainActor () -> Void)?
    public var onAppearance: (@MainActor (UIFont, UIColor) -> Void)?
    public var onViewportChange: (@MainActor (NativeTerminalViewport) -> Void)?
    public var onFailure: (@MainActor (TerminalRenderError) -> Void)?
    /// Each decoded frame as it is admitted, before it is drawn.
    public var onFrameAdmitted: (@MainActor (TerminalRenderFrame) -> Void)?
    public private(set) var configuration: NativeTerminalRenderConfiguration

    private let commandQueue: any MTLCommandQueue
    private let backgroundPipeline: any MTLRenderPipelineState
    private let textPipeline: any MTLRenderPipelineState
    private let worker: NativeTerminalPreparation
    private var slots = TerminalRenderSlots()
    private var submitted: [TerminalRenderSlots.Lease: any MTLCommandBuffer] = [:]
    private var prepared: NativePreparedTerminalFrame?
    private var pending: Data?
    private var latest: TerminalRenderFrame?
    private var preparing = false
    private var preparationGeneration: UInt64 = 1
    private var deliveredCaret: UInt64 = 0
    private var lastAdmittedRevision: UInt64 = 0
    private var active = true
    private var retired = false
    private var needsPreparation = false
    private var waitingForResources = false
    private var failedRevision: UInt64?
    private var lastSize = CGSize.zero
    private var lastScale: CGFloat = 0
    private var lastViewport: NativeTerminalViewport?
    private var blinkPhase: UInt64 = 0
    private var blinkTimer: Timer?
    private var displayLink: CADisplayLink?
    private var displayLifetime: NativeTerminalDisplayLinkLifetime?
    private lazy var displayTarget = NativeTerminalDisplayTarget(self)

    public init(source: TerminalSource, bundle: Bundle = .main,
                configuration: NativeTerminalRenderConfiguration = .init()) async throws {
        try configuration.validate()
        let pipelines = try await NativeTerminalPipelineLoader.shared.load(bundle: bundle)
        try Task.checkCancellation()
        guard let queue = pipelines.device.makeCommandQueue() else { throw TerminalRenderError.unavailable }
        backgroundPipeline = pipelines.background; textPipeline = pipelines.text
        self.source = source; self.configuration = configuration; commandQueue = queue
        worker = NativeTerminalPreparation(device: pipelines.device)
        super.init(frame: .zero, device: pipelines.device)
        colorPixelFormat = .bgra8Unorm_srgb; framebufferOnly = true
        isPaused = true; enableSetNeedsDisplay = false; autoResizeDrawable = true
        isOpaque = true; backgroundColor = .black
        isAccessibilityElement = true; accessibilityLabel = "Terminal output"
        accessibilityTraits = .updatesFrequently
        NotificationCenter.default.addObserver(self, selector: #selector(applicationDeactivated),
            name: UIApplication.willResignActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationBackgrounded),
            name: UIApplication.didEnterBackgroundNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationActivated),
            name: UIApplication.didBecomeActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(sceneDeactivated(_:)),
            name: UIScene.willDeactivateNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(sceneActivated(_:)),
            name: UIScene.didActivateNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(resourcesBecameAvailable),
            name: NativeTerminalGPUReservation.availabilityNotification, object: nil)
    }
    @available(*, unavailable)
    public required init(coder: NSCoder) { fatalError("Use init(source:bundle:configuration:)") }
    deinit { NotificationCenter.default.removeObserver(self) }

    /// The last admitted frame: what is shown, or about to be.
    public var admittedFrame: TerminalRenderFrame? { latest }

    /// The last admitted screen, read on demand so steady output costs nothing.
    public override var accessibilityValue: String? {
        get { latest?.readableText ?? super.accessibilityValue }
        set { super.accessibilityValue = newValue }
    }

    /// Complete replacements can supersede an unprepared frame. No byte stream
    /// or partial terminal delta is dropped or coalesced here.
    public func submit(_ bytes: Data, source: TerminalSource) throws {
        guard source == self.source else { throw TerminalRenderError.staleSource }
        guard !retired else { throw TerminalRenderError.unavailable }
        guard bytes.count <= TerminalRenderLimits().maximumFrameBytes else { throw TerminalRenderError.capacity }
        // Decode and all semantic validation are performed by the single worker.
        pending = bytes
        failedRevision = nil
        if prepared != nil { requestDisplay() }
        requestPreparation()
    }

    public func updateConfiguration(_ configuration: NativeTerminalRenderConfiguration) throws {
        try configuration.validate()
        guard !retired else { throw TerminalRenderError.unavailable }
        guard self.configuration != configuration else { return }
        guard configuration.fontRevision > self.configuration.fontRevision else { throw TerminalRenderError.staleFrame }
        self.configuration = configuration
        try invalidatePreparation()
        requestPreparation()
    }

    public func setSceneActive(_ active: Bool) {
        guard !retired, self.active != active else { return }
        self.active = active
        worker.setActive(active)
        if active { requestPreparation() }
        else {
            waitingForResources = false
            blinkTimer?.invalidate(); blinkTimer = nil
            displayLink?.isPaused = true
            discardPrepared()
            onCaretUnavailable?()
            if preparationGeneration < UInt64.max { preparationGeneration += 1 }
            else { retire() }
        }
    }

    /// GPU-owned slot resources remain retained until their exact command buffer
    /// completes. Retirement invalidates future callbacks, not GPU ownership.
    public func retire() {
        guard !retired else { return }
        retired = true; active = false; pending = nil; latest = nil
        blinkTimer?.invalidate(); blinkTimer = nil
        displayLink?.invalidate(); displayLink = nil; displayLifetime = nil
        discardPrepared()
        worker.setActive(false)
        onCaretUnavailable?()
        onCaret = nil; onCaretUnavailable = nil; onAppearance = nil; onViewportChange = nil; onFailure = nil
        onFrameAdmitted = nil
    }

    public override func layoutSubviews() {
        super.layoutSubviews()
        let scale = window?.screen.scale ?? contentScaleFactor
        if lastSize != drawableSize || lastScale != scale {
            lastSize = drawableSize; lastScale = scale
            do { try invalidatePreparation(); requestPreparation() }
            catch { fail(error) }
        }
    }

    public override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil { setSceneActive(false) }
        else { setSceneActive(window?.windowScene?.activationState == .foregroundActive) }
    }

    private func invalidatePreparation() throws {
        guard preparationGeneration < UInt64.max else { throw TerminalRenderError.exhausted }
        preparationGeneration += 1; discardPrepared(); failedRevision = nil
        blinkTimer?.invalidate(); blinkTimer = nil
        onCaretUnavailable?()
    }

    private func discardPrepared() {
        if let prepared { try? slots.abandon(prepared.lease) }
        prepared = nil
    }

    private func requestPreparation() {
        needsPreparation = true
        startPreparation()
    }

    private func startPreparation() {
        guard active, !retired, !preparing, prepared == nil, needsPreparation,
              window != nil, drawableSize.width > 0, drawableSize.height > 0,
              drawableSize.width <= 16384, drawableSize.height <= 16384,
              pending != nil || latest != nil else { return }
        // A busy GPU budget is transient pressure, not a broken terminal. Keep
        // one latest snapshot and wait for actual resource retirement. Recheck
        // here as well as on wake so a release before the callback is not lost.
        guard !waitingForResources || NativeTerminalGPUReservation.hasCapacity else { return }
        do {
            guard let lease = try slots.acquire() else { return }
            waitingForResources = false
            let packet = pending; pending = nil
            let retained = latest
            let generation = preparationGeneration
            let viewport = SIMD2<Int>(Int(drawableSize.width), Int(drawableSize.height))
            let scale = Double(window?.screen.scale ?? contentScaleFactor)
            let config = configuration, expected = source
            let phase = blinkPhase, minimumRevision = lastAdmittedRevision
            needsPreparation = false; preparing = true
            worker.enqueue(packet: packet, retained: retained, source: expected, lease: lease,
                generation: generation, viewport: viewport, scale: scale, configuration: config,
                blinkPhase: phase, minimumRevision: minimumRevision) { [weak self] result in
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    self.preparing = false
                    switch result {
                    case let .success(result):
                        let semantic = result.semantic
                        if !self.retired {
                            if self.latest?.revision != semantic.revision {
                                self.blinkTimer?.invalidate(); self.blinkTimer = nil
                            }
                            self.latest = semantic
                            self.lastAdmittedRevision = semantic.revision
                            self.onFrameAdmitted?(semantic)
                        }
                        guard case let .ready(frame) = result else {
                            try? self.slots.abandon(lease)
                            if !self.retired, generation == self.preparationGeneration {
                                self.blinkTimer?.invalidate(); self.blinkTimer = nil
                                if case let .unrenderable(_, error) = result {
                                    self.failedRevision = semantic.revision
                                    self.onCaretUnavailable?(); self.fail(error)
                                } else if case .waitingForResources = result {
                                    self.waitingForResources = true
                                    self.needsPreparation = true
                                    self.onCaretUnavailable?()
                                }
                            }
                            if self.pending != nil { self.needsPreparation = true }
                            self.startPreparation(); return
                        }
                        guard !self.retired, self.active, generation == self.preparationGeneration else {
                            try? self.slots.abandon(lease)
                            if !self.retired { self.needsPreparation = true }
                            self.startPreparation(); return
                        }
                        self.prepared = frame
                        let proposal = NativeTerminalViewport(source: self.source, revision: generation,
                            columns: frame.fittingColumns, rows: frame.fittingRows,
                            pixelWidth: frame.viewport.x, pixelHeight: frame.viewport.y)
                        if self.lastViewport != proposal {
                            self.lastViewport = proposal
                            self.onViewportChange?(proposal)
                        }
                        self.requestDisplay()
                    case let .failure(error):
                        try? self.slots.abandon(lease)
                        if !self.retired, generation == self.preparationGeneration {
                            self.fail(error)
                        }
                    }
                    if self.pending != nil { self.needsPreparation = true }
                    self.startPreparation()
                }
            }
        } catch { fail(error) }
    }

    private func requestDisplay() {
        guard active, !retired else { return }
        if displayLink == nil {
            let link = CADisplayLink(target: displayTarget, selector: #selector(NativeTerminalDisplayTarget.tick(_:)))
            link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 120, preferred: 60)
            displayLifetime = NativeTerminalDisplayLinkLifetime(link)
            link.add(to: .main, forMode: .common); displayLink = link
        }
        displayLink?.isPaused = false
    }

    fileprivate func displayTick() {
        displayLink?.isPaused = true
        guard active, !retired, prepared != nil else { return }
        draw()
    }

    public override func draw(_ rect: CGRect) {
        guard active, !retired, let frame = prepared,
              frame.generation == preparationGeneration, submitted.count < 1 else { return }
        guard let descriptor = currentRenderPassDescriptor, let drawable = currentDrawable,
              let commands = commandQueue.makeCommandBuffer()
        else {
            // A missing drawable keeps exactly one prepared frame. Retry only on
            // a later output/layout/lifecycle request, not an idle spinning loop.
            return
        }
        let clear = frame.clear
        descriptor.colorAttachments[0].clearColor = MTLClearColor(red: Double(clear[0]), green: Double(clear[1]), blue: Double(clear[2]), alpha: Double(clear[3]))
        descriptor.colorAttachments[0].loadAction = .clear
        guard Int(drawable.texture.width) == frame.viewport.x, Int(drawable.texture.height) == frame.viewport.y else {
            discardPrepared(); requestPreparation(); return
        }
        guard let encoder = commands.makeRenderCommandEncoder(descriptor: descriptor) else {
            fail(TerminalRenderError.unavailable); return
        }
        encoder.setViewport(MTLViewport(originX: 0, originY: 0, width: Double(frame.viewport.x), height: Double(frame.viewport.y), znear: 0, zfar: 1))
        encoder.setScissorRect(MTLScissorRect(x: 0, y: 0, width: frame.viewport.x, height: frame.viewport.y))
        encoder.setFragmentBuffer(frame.resources.uniforms, offset: 0, index: 0)
        encoder.setFragmentBuffer(frame.resources.background, offset: 0, index: 1)
        encoder.setRenderPipelineState(backgroundPipeline)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        if frame.instanceCount > 0 {
            encoder.setRenderPipelineState(textPipeline)
            encoder.setVertexBuffer(frame.resources.uniforms, offset: 0, index: 0)
            encoder.setVertexBuffer(frame.resources.text, offset: 0, index: 2)
            encoder.setFragmentTexture(frame.resources.gray, index: 0)
            encoder.setFragmentTexture(frame.resources.color, index: 1)
            encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4, instanceCount: frame.instanceCount)
        }
        encoder.endEncoding()
        do { try slots.submit(frame.lease) }
        catch { discardPrepared(); fail(error); return }
        prepared = nil; submitted[frame.lease] = commands
        commands.addCompletedHandler { [weak self, frame] command in
            let succeeded = command.status == .completed
            Task { @MainActor [weak self, frame] in
                guard let self else { return }
                self.submitted.removeValue(forKey: frame.lease)
                try? self.slots.complete(frame.lease)
                if !self.retired, self.active, frame.generation == self.preparationGeneration {
                    if succeeded {
                        if frame.lease.serial > self.deliveredCaret {
                            self.deliveredCaret = frame.lease.serial
                            let color = frame.semantic.palette[frame.semantic.reverseVideo ? 257 : 256]
                            self.onAppearance?(UIFont(name: frame.fontName, size: frame.fontPointSize)
                                ?? .monospacedSystemFont(ofSize: frame.fontPointSize, weight: .regular),
                                UIColor(red: CGFloat(color.red) / 255, green: CGFloat(color.green) / 255,
                                        blue: CGFloat(color.blue) / 255, alpha: CGFloat(color.alpha) / 255))
                            // Client callbacks may synchronously retire the
                            // surface or change its geometry/configuration.
                            if !self.retired, self.active, frame.generation == self.preparationGeneration {
                                if let caret = frame.caret {
                                    self.onCaret?(NativeTerminalCaret(source: self.source, revision: frame.lease.serial, rect: caret))
                                } else { self.onCaretUnavailable?() }
                            }
                        }
                        if frame.generation == self.preparationGeneration { self.armBlink(for: frame.semantic) }
                    } else { self.fail(TerminalRenderError.unavailable) }
                }
                if self.prepared != nil { self.requestDisplay() }
                self.startPreparation()
            }
        }
        commands.present(drawable)
        commands.commit()
        startPreparation()
    }

    private func armBlink(for frame: TerminalRenderFrame) {
        guard active, !retired, blinkTimer == nil, failedRevision != frame.revision,
              latest?.revision == frame.revision, (latest?.flags ?? 0) & 8 == 0,
              (configuration.cursorBlink && frame.cursor.visible && frame.cursor.blinks)
                || frame.lines.contains(where: { $0.cells.contains(where: { $0.blink != 0 }) }) else { return }
        // A discrete 250 ms clock supports rapid and slow blinking; it never
        // runs a continuous draw loop for a steady terminal.
        let generation = preparationGeneration, revision = frame.revision
        blinkTimer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: false) { [weak self] _ in
            Task { @MainActor [weak self] in
                guard let self, self.preparationGeneration == generation,
                      self.latest?.revision == revision else { return }
                self.blinkTimer = nil
                guard self.active, !self.retired, self.blinkPhase < UInt64.max,
                      let latest = self.latest, latest.flags & 8 == 0,
                      self.failedRevision != latest.revision else { return }
                self.blinkPhase += 1; self.requestPreparation()
            }
        }
    }

    private func fail(_ error: Error) { onFailure?((error as? TerminalRenderError) ?? .unavailable) }
    @objc private func resourcesBecameAvailable() {
        guard active, !retired, waitingForResources else { return }
        requestPreparation()
    }
    @objc private func applicationDeactivated() { setSceneActive(false) }
    @objc private func applicationActivated() { setSceneActive(window?.windowScene?.activationState == .foregroundActive) }
    @objc private func sceneDeactivated(_ note: Notification) {
        if let scene = note.object as? UIScene, scene === window?.windowScene { setSceneActive(false) }
    }
    @objc private func sceneActivated(_ note: Notification) {
        if let scene = note.object as? UIScene, scene === window?.windowScene { setSceneActive(true) }
    }
    @objc private func applicationBackgrounded() {
        setSceneActive(false)
        // Apple's required background scheduling barrier, never a per-frame GPU
        // completion wait. No preparation completion can recommence submission.
        for buffer in submitted.values where buffer.status == .committed || buffer.status == .enqueued {
            buffer.waitUntilScheduled()
        }
    }
}

@MainActor
private final class NativeTerminalDisplayTarget: NSObject {
    weak var view: NativeTerminalRenderView?
    init(_ view: NativeTerminalRenderView) { self.view = view }
    @objc func tick(_ sender: CADisplayLink) { view?.displayTick() }
}

/// Run loops retain even paused display links. Release that registration if a
/// caller drops its view without explicitly retiring it. Apple documents
/// CADisplayLink.invalidate() as thread-safe, so this immutable holder may
/// perform only that operation from a nonisolated deinitializer.
private final class NativeTerminalDisplayLinkLifetime: @unchecked Sendable {
    private let link: CADisplayLink
    init(_ link: CADisplayLink) { self.link = link }
    deinit { link.invalidate() }
}

private final class NativeTerminalPipelines: @unchecked Sendable {
    let device: any MTLDevice
    let background: any MTLRenderPipelineState, text: any MTLRenderPipelineState
    init(bundle: Bundle) throws {
        guard let device = MTLCreateSystemDefaultDevice() else { throw TerminalRenderError.unavailable }
        self.device = device
        let prelude = "constant float SRGB_TO_LINEAR[256] = {" + (0...255).map {
            String(TerminalRenderRGBA.linear(UInt8($0))) + "f"
        }.joined(separator: ",") + "};\n"
        func library(_ name: String) throws -> any MTLLibrary {
            guard let url = bundle.url(forResource: name, withExtension: "metal") else { throw TerminalRenderError.unavailable }
            let data = try Data(contentsOf: url, options: .mappedIfSafe)
            guard data.count <= 65536, let source = String(data: data, encoding: .utf8)
            else { throw TerminalRenderError.unavailable }
            return try device.makeLibrary(source: prelude + source, options: nil)
        }
        func pipeline(_ library: any MTLLibrary, vertex: String, fragment: String, blend: Bool) throws -> any MTLRenderPipelineState {
            let descriptor = MTLRenderPipelineDescriptor()
            descriptor.vertexFunction = library.makeFunction(name: vertex)
            descriptor.fragmentFunction = library.makeFunction(name: fragment)
            guard descriptor.vertexFunction != nil, descriptor.fragmentFunction != nil,
                  let attachment = descriptor.colorAttachments[0] else { throw TerminalRenderError.unavailable }
            attachment.pixelFormat = .bgra8Unorm_srgb; attachment.isBlendingEnabled = blend
            attachment.sourceRGBBlendFactor = .one; attachment.sourceAlphaBlendFactor = .one
            attachment.destinationRGBBlendFactor = .oneMinusSourceAlpha
            attachment.destinationAlphaBlendFactor = .oneMinusSourceAlpha
            return try device.makeRenderPipelineState(descriptor: descriptor)
        }
        background = try pipeline(library("background"), vertex: "bg_vertex", fragment: "bg_fragment", blend: false)
        text = try pipeline(library("text"), vertex: "text_vertex", fragment: "text_fragment", blend: true)
    }
}

/// One process-wide pipeline creation operation, with four coalesced callers at
/// most. Shader compilation and bundle I/O never execute on MainActor. A cancelled
/// caller retains its reservation until the native operation really returns.
private final class NativeTerminalPipelineLoader: @unchecked Sendable {
    static let shared = NativeTerminalPipelineLoader()
    private let lock = NSLock()
    private let queue = DispatchQueue(label: "org.frankenterm.terminal.pipelines", qos: .userInitiated)
    private var cached: NativeTerminalPipelines?
    private var bundleURL: URL?
    private var waiters: [CheckedContinuation<NativeTerminalPipelines, any Error>] = []
    func load(bundle: Bundle) async throws -> NativeTerminalPipelines {
        try Task.checkCancellation()
        return try await withCheckedThrowingContinuation { continuation in
            let launch: Bool = lock.withLock {
                if let cached {
                    if bundleURL == bundle.bundleURL { continuation.resume(returning: cached) }
                    else { continuation.resume(throwing: TerminalRenderError.unavailable) }
                    return false
                }
                if !waiters.isEmpty, bundleURL != bundle.bundleURL {
                    continuation.resume(throwing: TerminalRenderError.unavailable); return false
                }
                guard waiters.count < 4 else { continuation.resume(throwing: TerminalRenderError.capacity); return false }
                let launch = waiters.isEmpty
                waiters.append(continuation); bundleURL = bundle.bundleURL
                return launch
            }
            if launch {
                queue.async { [self] in
                    let result = Result { try NativeTerminalPipelines(bundle: bundle) }
                    let callbacks = lock.withLock {
                        if case let .success(value) = result { cached = value }
                        let callbacks = waiters; waiters.removeAll(keepingCapacity: true)
                        return callbacks
                    }
                    callbacks.forEach { $0.resume(with: result) }
                }
            }
        }
    }
}

/// These resources cross executors only after preparation has finished. Their
/// sole writer is the serial worker, admitted again only after the exact lease's
/// command-buffer completion. Metal retains them for each command as well.
private final class NativeTerminalResources: @unchecked Sendable {
    private let reservation: NativeTerminalGPUReservation
    let uniforms: any MTLBuffer, background: any MTLBuffer, text: any MTLBuffer
    let gray: any MTLTexture, color: any MTLTexture
    static let atlasSide = 1024
    static let maximumInstances = 131072
    init(device: any MTLDevice) throws {
        let reservation = try NativeTerminalGPUReservation()
        guard let uniforms = device.makeBuffer(length: 256, options: .storageModeShared),
              let background = device.makeBuffer(length: 65536 * 4, options: .storageModeShared),
              let text = device.makeBuffer(length: Self.maximumInstances * 24, options: .storageModeShared)
        else { throw TerminalRenderError.unavailable }
        func texture(_ format: MTLPixelFormat) -> (any MTLTexture)? {
            let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: Self.atlasSide, height: Self.atlasSide, mipmapped: false)
            d.storageMode = .shared; d.usage = .shaderRead
            return device.makeTexture(descriptor: d)
        }
        guard let gray = texture(.r8Unorm), let color = texture(.bgra8Unorm_srgb)
        else { throw TerminalRenderError.unavailable }
        self.uniforms = uniforms; self.background = background; self.text = text
        self.gray = gray; self.color = color
        self.reservation = reservation
    }
}

/// Six complete slot sets across all scenes bound these allocations to about
/// 50 MiB. A slot retained by an unfinished command still consumes its permit.
private enum NativeTerminalResourceError: Error { case temporarilyUnavailable }

private final class NativeTerminalGPUReservation: Sendable {
    static let availabilityNotification = Notification.Name("org.frankenterm.terminal.gpu-resources-available")
    private static let budget = TerminalRenderResourceBudget(maximumResources: 6) {
        // The budget coalesces release bursts and calls this on MainActor.
        NotificationCenter.default.post(name: availabilityNotification, object: nil)
    }
    private let lease: TerminalRenderResourceBudget.Lease
    static var hasCapacity: Bool { budget.hasCapacity }
    init() throws {
        guard let lease = Self.budget.acquire() else {
            throw NativeTerminalResourceError.temporarilyUnavailable
        }
        self.lease = lease
    }
}

private struct NativePreparedTerminalFrame: @unchecked Sendable {
    let semantic: TerminalRenderFrame
    let lease: TerminalRenderSlots.Lease, generation: UInt64
    let resources: NativeTerminalResources
    let viewport: SIMD2<Int>
    let instanceCount: Int
    let clear: [Float]
    let caret: CGRect?
    let fontName: String
    let fontPointSize: CGFloat
    let fittingColumns: Int, fittingRows: Int
}

private enum NativeTerminalPreparationResult: Sendable {
    case ready(NativePreparedTerminalFrame)
    case synchronized(TerminalRenderFrame)
    case inactive(TerminalRenderFrame)
    case waitingForResources(TerminalRenderFrame)
    case unrenderable(TerminalRenderFrame, TerminalRenderError)
    var semantic: TerminalRenderFrame {
        switch self {
        case let .ready(value): value.semantic
        case let .synchronized(value), let .inactive(value), let .waitingForResources(value),
             let .unrenderable(value, _): value
        }
    }
}

private final class NativeTerminalPreparation: @unchecked Sendable {
    private let queue = DispatchQueue(label: "org.frankenterm.terminal.render", qos: .userInteractive)
    private let device: any MTLDevice
    private var resources: [Int: NativeTerminalResources] = [:]
    private let activityLock = NSLock()
    private var active = true, releaseQueued = false
    init(device: any MTLDevice) { self.device = device }
    func setActive(_ active: Bool) {
        let release = activityLock.withLock {
            self.active = active
            guard !active, !releaseQueued else { return false }
            releaseQueued = true; return true
        }
        if release {
            queue.async { [self] in
                activityLock.withLock { releaseQueued = false }
                // Cache ownership only. Prepared results and submitted GPU
                // commands keep their exact resources/permits until completion.
                resources.removeAll()
            }
        }
    }
    func enqueue(packet: Data?, retained: TerminalRenderFrame?, source: TerminalSource,
                 lease: TerminalRenderSlots.Lease, generation: UInt64, viewport: SIMD2<Int>,
                 scale: Double, configuration: NativeTerminalRenderConfiguration, blinkPhase: UInt64,
                 minimumRevision: UInt64,
                 completion: @escaping @Sendable (Result<NativeTerminalPreparationResult, TerminalRenderError>) -> Void) {
        queue.async { [self] in
            autoreleasepool {
                do {
                    let frame: TerminalRenderFrame
                    if let packet {
                        frame = try TerminalRenderFrame.decode(packet, expectedSource: source)
                        guard frame.revision > minimumRevision else { throw TerminalRenderError.staleFrame }
                        if let retained { try frame.validateSuccessor(of: retained) }
                    } else if let retained { frame = retained }
                    else { throw TerminalRenderError.unavailable }
                    guard frame.rows * frame.columns <= 65536 else { throw TerminalRenderError.capacity }
                    guard activityLock.withLock({ active }) else {
                        // A coalesced eviction may precede this queued job.
                        // Do not repopulate an inactive surface's cache afterward.
                        resources.removeAll()
                        completion(.success(.inactive(frame))); return
                    }
                    if frame.flags & 8 != 0 { completion(.success(.synchronized(frame))); return }
                    do {
                        guard frame.fontRevision == configuration.fontRevision else { throw TerminalRenderError.staleFrame }
                        let owned: NativeTerminalResources
                        if let existing = resources[lease.slot] { owned = existing }
                        else {
                            owned = try NativeTerminalResources(device: device)
                            resources[lease.slot] = owned
                        }
                        let result = try NativeTerminalFrameBuilder(frame: frame, resources: owned,
                            configuration: configuration, scale: scale, viewport: viewport, blinkPhase: blinkPhase)
                            .build(lease: lease, generation: generation)
                        completion(.success(.ready(result)))
                    } catch NativeTerminalResourceError.temporarilyUnavailable {
                        // Another slot may be cached but idle while this slot
                        // needs allocation. Drop cache references so our own
                        // idle permits cannot deadlock the shared budget. A
                        // submitted frame still owns its resources until its
                        // real completion, which supplies the availability wake.
                        resources.removeAll()
                        completion(.success(.waitingForResources(frame)))
                    } catch {
                        completion(.success(.unrenderable(frame, (error as? TerminalRenderError) ?? .unavailable)))
                    }
                } catch { completion(.failure((error as? TerminalRenderError) ?? .unavailable)) }
            }
        }
    }
}

/// Each preparation owns all Core Text layout objects and a bounded per-frame
/// glyph cache. Atlas allocation never outlives its exact slot lease; rebuilding
/// a slot resets its allocator only after that slot's previous GPU work retires.
private final class NativeTerminalFrameBuilder {
    private let frame: TerminalRenderFrame, resources: NativeTerminalResources
    private let configuration: NativeTerminalRenderConfiguration, scale: Double
    private let viewport: SIMD2<Int>, blinkPhase: UInt64
    private let baseFont: CTFont
    /// Only kCTFontCascadeListAttribute; reapplied to every derived face, since
    /// copying a font with new symbolic traits need not keep a custom cascade.
    private let cascade: CTFontDescriptor?
    private let cellWidth: Double, cellHeight: Double, ascent: Double
    private var instances = [UInt8]()
    private var fonts = [CTFont]()
    private var glyphs: [GlyphKey: PlacedGlyph] = [:]
    private var grayShelf = Shelf(), colorShelf = Shelf()
    private struct GlyphKey: Hashable { let font: Int; let glyph: CGGlyph }
    private struct PlacedGlyph {
        let color: Bool, x: Int, y: Int, width: Int, height: Int, left: Int, top: Int
    }
    private struct Shelf {
        var x = 0, y = 0, height = 0
        mutating func reserve(width: Int, height: Int) throws -> (Int, Int) {
            let side = NativeTerminalResources.atlasSide
            guard width <= side, height <= side else { throw TerminalRenderError.capacity }
            if x + width > side { y += self.height; x = 0; self.height = 0 }
            guard y + height <= side else { throw TerminalRenderError.capacity }
            let result = (x,y); x += width; self.height = max(self.height, height)
            return result
        }
    }

    init(frame: TerminalRenderFrame, resources: NativeTerminalResources,
         configuration: NativeTerminalRenderConfiguration, scale: Double,
         viewport: SIMD2<Int>, blinkPhase: UInt64) throws {
        guard scale.isFinite, (1...4).contains(scale) else { throw TerminalRenderError.capacity }
        self.frame = frame; self.resources = resources; self.configuration = configuration
        self.scale = scale; self.viewport = viewport; self.blinkPhase = blinkPhase
        let size = configuration.fontPointSize * scale
        // An explicit cascade of the configured fallbacks that are actually
        // installed: an unknown name is skipped rather than letting Core Text
        // substitute an unrelated face for it.
        let installed = configuration.fallbackFontNames.compactMap { name -> CTFontDescriptor? in
            let candidate = CTFontCreateWithName(name as CFString, size, nil)
            guard CTFontCopyPostScriptName(candidate) as String == name else { return nil }
            return CTFontDescriptorCreateWithNameAndSize(name as CFString, size)
        }
        cascade = installed.isEmpty ? nil : CTFontDescriptorCreateWithAttributes(
            [kCTFontCascadeListAttribute: installed] as CFDictionary)
        let primary = CTFontCreateWithName(configuration.fontName as CFString, size, nil)
        baseFont = cascade.map { CTFontCreateCopyWithAttributes(primary, size, nil, $0) } ?? primary
        var character: UniChar = 77, glyph: CGGlyph = 0, advance = CGSize.zero
        guard CTFontGetGlyphsForCharacters(baseFont, &character, &glyph, 1) else { throw TerminalRenderError.unavailable }
        CTFontGetAdvancesForGlyphs(baseFont, .horizontal, &glyph, &advance, 1)
        cellWidth = ceil(advance.width)
        ascent = ceil(CTFontGetAscent(baseFont))
        cellHeight = ceil(CTFontGetAscent(baseFont) + CTFontGetDescent(baseFont) + CTFontGetLeading(baseFont))
        guard cellWidth.isFinite, cellHeight.isFinite, cellWidth >= 1, cellHeight >= 1,
              cellWidth <= 256, cellHeight <= 512 else { throw TerminalRenderError.capacity }
    }

    func build(lease: TerminalRenderSlots.Lease, generation: UInt64) throws -> NativePreparedTerminalFrame {
        var backgrounds = [UInt8](repeating: 0, count: frame.rows * frame.columns * 4)
        instances.reserveCapacity(min(NativeTerminalResources.maximumInstances * 24, frame.rows * frame.columns * 24))
        for (rowIndex, row) in frame.lines.enumerated() {
            for cell in row.cells {
                let colors = paint(cell, stableRow: row.stableIndex)
                if colors.background != frame.palette[frame.reverseVideo ? 256 : 257] {
                    for column in cell.column..<(cell.column + cell.width) {
                        let at = (rowIndex * frame.columns + column) * 4
                        backgrounds.replaceSubrange(at..<(at + 4), with: [colors.background.red, colors.background.green, colors.background.blue, 1])
                    }
                }
                if !cell.invisible {
                    let flags = UInt8(cell.underlineStyle << 3) | (cell.overline ? 128 : 0)
                    if flags != 0 {
                        for column in cell.column..<(cell.column + cell.width) {
                            let paint = self.paint(cell, stableRow: row.stableIndex, column: column)
                            try append(TerminalRenderPacking.text(column: column, row: rowIndex, width: 1,
                                foreground: paint.foreground, underline: paint.underline, flags: flags))
                        }
                    }
                }
            }
            try shape(row, rowIndex: rowIndex)
            // Match the canonical shader layering: underline/overline below
            // glyphs; strikethrough above them.
            for cell in row.cells where cell.strikethrough && !cell.invisible {
                for column in cell.column..<(cell.column + cell.width) {
                    let colors = paint(cell, stableRow: row.stableIndex, column: column)
                    try append(TerminalRenderPacking.text(column: column, row: rowIndex, width: 1,
                        foreground: colors.foreground, underline: colors.underline, flags: 64))
                }
            }
        }
        var uniform = TerminalRenderUniforms()
        uniform.frame = lease.serial
        uniform.viewport = (UInt32(viewport.x), UInt32(viewport.y))
        uniform.rows = UInt32(frame.rows); uniform.columns = UInt32(frame.columns)
        uniform.clear = frame.palette[frame.reverseVideo ? 256 : 257]
        uniform.cellSize = (Float(cellWidth), Float(cellHeight))
        let cursorDelta = frame.cursor.stableRow.subtractingReportingOverflow(frame.viewportFirst)
        let cursorRow = cursorDelta.partialValue
        let inside = !cursorDelta.overflow && cursorRow >= 0 && cursorRow < Int64(frame.rows)
        let showCursor = frame.cursor.visible && inside
            && (!configuration.cursorBlink || !frame.cursor.blinks || (blinkPhase / 2) % 2 == 0)
        var caret: CGRect?
        if inside, Double(frame.cursor.column + 1) * cellWidth <= Double(viewport.x),
           Double(cursorRow + 1) * cellHeight <= Double(viewport.y) {
            caret = CGRect(x: Double(frame.cursor.column) * cellWidth / scale,
                y: Double(cursorRow) * cellHeight / scale, width: cellWidth / scale, height: cellHeight / scale)
        }
        if showCursor {
            let shape: UInt32 = [3,4].contains(frame.cursor.shape) ? 3 : [5,6].contains(frame.cursor.shape) ? 4 : 1
            uniform.cursorShape = shape
            uniform.cursorColumn = UInt32(frame.cursor.column); uniform.cursorRow = UInt32(cursorRow)
            uniform.cursorColor = frame.palette[259]
            uniform.cursorThickness = Float(max(1, scale))
            uniform.cursorWidth = UInt32(min(frame.cursor.width, frame.columns - frame.cursor.column))
        }
        uniform.underlinePosition = Float(ascent - Double(CTFontGetUnderlinePosition(baseFont)))
        uniform.lineThickness = Float(max(1, Double(CTFontGetUnderlineThickness(baseFont))))
        uniform.strikethroughPosition = Float(ascent * 0.6)
        let uniforms = try TerminalRenderPacking.uniforms(uniform)
        uniforms.withUnsafeBytes { resources.uniforms.contents().copyMemory(from: $0.baseAddress!, byteCount: uniforms.count) }
        backgrounds.withUnsafeBytes { resources.background.contents().copyMemory(from: $0.baseAddress!, byteCount: backgrounds.count) }
        if !instances.isEmpty {
            instances.withUnsafeBytes { resources.text.contents().copyMemory(from: $0.baseAddress!, byteCount: instances.count) }
        }
        let fittingColumns = max(1, min(512, Int(Double(viewport.x) / cellWidth)))
        return NativePreparedTerminalFrame(semantic: frame, lease: lease, generation: generation,
            resources: resources, viewport: viewport, instanceCount: instances.count / 24,
            clear: uniform.clear.linearPremultiplied, caret: caret,
            fontName: CTFontCopyPostScriptName(baseFont) as String, fontPointSize: CGFloat(configuration.fontPointSize),
            fittingColumns: fittingColumns,
            fittingRows: max(1, min(512, 65536 / fittingColumns, Int(Double(viewport.y) / cellHeight))))
    }

    private func append(_ bytes: [UInt8]) throws {
        guard instances.count <= (NativeTerminalResources.maximumInstances - 1) * 24 else { throw TerminalRenderError.capacity }
        instances.append(contentsOf: bytes)
    }

    private func paint(_ cell: TerminalRenderCell, stableRow: Int64, column: Int? = nil) -> (foreground: TerminalRenderRGBA, background: TerminalRenderRGBA, underline: TerminalRenderRGBA) {
        var fg = cell.foreground.resolve(frame.palette, fallback: 256,
            boldBright: configuration.boldBrightensANSI && cell.intensity == 1)
        var bg = cell.background.resolve(frame.palette, fallback: 257)
        if cell.reverse != frame.reverseVideo { swap(&fg, &bg) }
        var underline = cell.underline == .default ? fg : cell.underline.resolve(frame.palette, fallback: 256)
        let hiddenBlink = cell.blink == 1 && (blinkPhase / 2) % 2 != 0 || cell.blink == 2 && blinkPhase % 2 != 0
        if hiddenBlink {
            fg = TerminalRenderRGBA(fg.red, fg.green, fg.blue, 0)
            underline = TerminalRenderRGBA(underline.red, underline.green, underline.blue, 0)
        }
        if cell.intensity == 2 {
            fg = TerminalRenderRGBA(fg.red, fg.green, fg.blue, fg.alpha / 2)
            underline = TerminalRenderRGBA(underline.red, underline.green, underline.blue, underline.alpha / 2)
        }
        let onCursor = stableRow == frame.cursor.stableRow && frame.cursor.visible && [0,1,2].contains(frame.cursor.shape)
            && (!configuration.cursorBlink || !frame.cursor.blinks || (blinkPhase / 2) % 2 == 0)
            && (column ?? cell.column) >= frame.cursor.column
            && (column ?? cell.column) < frame.cursor.column + frame.cursor.width
        // The canonical Metal scene applies focused block-cursor colors after
        // text blinking; the cursor deliberately reveals its covered glyph.
        // Concealed cells remain omitted independently of this paint decision.
        if onCursor, frame.palette[258].alpha > 0 { fg = frame.palette[258] }
        return (fg,bg,underline)
    }

    private func font(for cell: TerminalRenderCell) throws -> CTFont {
        var traits: CTFontSymbolicTraits = []
        if cell.intensity == 1 { traits.insert(.traitBold) }
        if cell.italic { traits.insert(.traitItalic) }
        let size = configuration.fontPointSize * scale * (cell.verticalAlignment == 0 ? 1 : 0.75)
        let derived: CTFont
        if let font = CTFontCreateCopyWithSymbolicTraits(baseFont, size, nil, traits, [.traitBold,.traitItalic]) {
            derived = font
        } else {
            guard traits.isEmpty else { throw TerminalRenderError.unsupported }
            derived = CTFontCreateCopyWithAttributes(baseFont, size, nil, nil)
        }
        return cascade.map { CTFontCreateCopyWithAttributes(derived, size, nil, $0) } ?? derived
    }

    private func shape(_ row: TerminalRenderRow, rowIndex: Int) throws {
        guard !row.cells.isEmpty else { return }
        let layout = try TerminalRenderTextLayout(row: row)
        let attributed = NSMutableAttributedString(string: layout.text, attributes: [
            NSAttributedString.Key(kCTFontAttributeName as String): baseFont,
            NSAttributedString.Key(kCTLigatureAttributeName as String): 1,
            // FTSF v1 admits only canonical LTR, bidi-disabled row layouts.
            // Core Text must not independently reorder terminal columns.
            NSAttributedString.Key(kCTWritingDirectionAttributeName as String):
                [NSNumber(value: NSWritingDirection.leftToRight.rawValue | NSWritingDirectionFormatType.override.rawValue)]
        ])
        for (index, cell) in row.cells.enumerated() {
            let range = layout.ranges[index]
            attributed.addAttribute(NSAttributedString.Key(kCTFontAttributeName as String), value: try font(for: cell),
                range: NSRange(location: range.lowerBound, length: range.count))
        }
        let line = CTLineCreateWithAttributedString(attributed as CFAttributedString)
        let runs = CTLineGetGlyphRuns(line) as! [CTRun]
        struct ShapedGlyph { let font: CTFont; let glyph: CGGlyph; let position: CGPoint; let index: Int }
        var shaped = [ShapedGlyph]()
        var totalGlyphs = 0
        var previousCell: Int?
        for run in runs {
            guard !CTRunGetStatus(run).contains(.rightToLeft) else { throw TerminalRenderError.unsupported }
            let count = CTRunGetGlyphCount(run)
            guard count >= 0, count <= NativeTerminalResources.maximumInstances - totalGlyphs else { throw TerminalRenderError.capacity }
            totalGlyphs += count
            let attributes = CTRunGetAttributes(run) as NSDictionary
            guard let runFont = attributes[kCTFontAttributeName] else { throw TerminalRenderError.unavailable }
            let font = runFont as! CTFont
            let matrix = CTRunGetTextMatrix(run)
            guard matrix.isIdentity else { throw TerminalRenderError.unsupported }
            var ids = [CGGlyph](repeating: 0, count: count)
            var positions = [CGPoint](repeating: .zero, count: count)
            var indices = [CFIndex](repeating: 0, count: count)
            CTRunGetGlyphs(run, CFRange(location: 0, length: count), &ids)
            CTRunGetPositions(run, CFRange(location: 0, length: count), &positions)
            CTRunGetStringIndices(run, CFRange(location: 0, length: count), &indices)
            for index in 0..<count {
                let stringIndex = indices[index]
                guard stringIndex >= 0, stringIndex < attributed.length,
                      positions[index].x.isFinite, positions[index].y.isFinite,
                      abs(positions[index].x) <= 1_048_576, abs(positions[index].y) <= 32768
                else { throw TerminalRenderError.unsupported }
                if let cell = layout.cellIndices[stringIndex] {
                    // Reordered marks inside one canonical grapheme retain
                    // Core Text positions. Reordering separate terminal cells
                    // requires a future explicit layout contract.
                    if let previousCell, cell < previousCell { throw TerminalRenderError.unsupported }
                    previousCell = cell
                }
                shaped.append(ShapedGlyph(font: font, glyph: ids[index], position: positions[index], index: stringIndex))
            }
        }
        // Anchor to the canonical cell, not the individual Core Text string
        // index. Combining glyphs may have different indices inside one cell.
        var anchors: [Int: (index: Int, point: CGPoint)] = [:]
        for glyph in shaped {
            guard let cell = layout.cellIndices[glyph.index] else { continue }
            if anchors[cell] == nil || glyph.index < anchors[cell]!.index {
                anchors[cell] = (glyph.index, glyph.position)
            }
        }
        let boundaries = Array(Set(shaped.map(\.index) + [attributed.length])).sorted()
        var lastCellForCluster: [Int: Int] = [:]
        for pair in zip(boundaries, boundaries.dropFirst()) {
            // Disjoint intervals bound this scan to the row's UTF-16 length,
            // even for thousands of marks sharing one shaping cluster.
            for cell in layout.cellIndices[pair.0..<pair.1] {
                if let cell { lastCellForCluster[pair.0] = max(lastCellForCluster[pair.0] ?? cell, cell) }
            }
        }
        for glyph in shaped {
            guard let cellIndex = layout.cellIndices[glyph.index], let anchor = anchors[cellIndex] else { continue }
            let cell = row.cells[cellIndex]
            let endCell = max(cellIndex, lastCellForCluster[glyph.index] ?? cellIndex)
            let placed = try rasterize(glyph.glyph, font: glyph.font)
            guard placed.width > 0, placed.height > 0 else { continue }
            let vertical = cell.verticalAlignment == 1 ? -cellHeight * 0.2 : cell.verticalAlignment == 2 ? cellHeight * 0.15 : 0
            let offsetX = Int((glyph.position.x - anchor.point.x).rounded()) + placed.left
            let offsetY = Int((ascent + vertical - (glyph.position.y - anchor.point.y)).rounded()) - placed.top
            let absoluteLeft = cell.column * Int(cellWidth) + offsetX
            // Preserve shaping context across paint changes, but sample only
            // the glyph fragment belonging to each authoritative cell's paint.
            // Outer overhangs remain with the originating cluster's edge cells.
            for index in cellIndex...endCell {
                let part = row.cells[index]
                if part.invisible { continue }
                for column in part.column..<(part.column + part.width) {
                    let partLeft = column * Int(cellWidth)
                    let partRight = (column + 1) * Int(cellWidth)
                    let first = index == cellIndex && column == cell.column
                    let last = index == endCell && column == part.column + part.width - 1
                    let low = first ? 0 : max(0, partLeft - absoluteLeft)
                    let high = last ? placed.width : min(placed.width, partRight - absoluteLeft)
                    guard high > low else { continue }
                    let paint = self.paint(part, stableRow: row.stableIndex, column: column)
                    try append(TerminalRenderPacking.text(column: column, row: rowIndex, width: 1,
                        foreground: paint.foreground, underline: paint.underline, flags: placed.color ? 2 : 1,
                        atlas: (placed.x + low,placed.y,high - low,placed.height),
                        offset: (absoluteLeft + low - partLeft,offsetY)))
                }
            }
        }
    }

    private func rasterize(_ glyph: CGGlyph, font: CTFont) throws -> PlacedGlyph {
        let fontIndex: Int
        if let existing = fonts.firstIndex(where: { CFEqual($0, font) }) { fontIndex = existing }
        else {
            guard fonts.count < 256 else { throw TerminalRenderError.capacity }
            fontIndex = fonts.count; fonts.append(font)
        }
        let key = GlyphKey(font: fontIndex, glyph: glyph)
        if let cached = glyphs[key] { return cached }
        var glyph = glyph
        let bounds = CTFontGetBoundingRectsForGlyphs(font, .horizontal, &glyph, nil, 1)
        guard !bounds.isNull, bounds.minX.isFinite, bounds.minY.isFinite,
              bounds.width.isFinite, bounds.height.isFinite, abs(bounds.minX) <= 4096, abs(bounds.minY) <= 4096,
              bounds.width <= 512, bounds.height <= 512 else { throw TerminalRenderError.capacity }
        if bounds.isEmpty { return PlacedGlyph(color: false, x: 0, y: 0, width: 0, height: 0, left: 0, top: 0) }
        let left = Int(floor(bounds.minX)) - 1, bottom = Int(floor(bounds.minY)) - 1
        let top = Int(ceil(bounds.maxY)) + 1, right = Int(ceil(bounds.maxX)) + 1
        let width = right - left, height = top - bottom
        guard (1...514).contains(width), (1...514).contains(height), glyphs.count < 16384 else { throw TerminalRenderError.capacity }
        let color = CTFontGetSymbolicTraits(font).contains(.traitColorGlyphs)
        let bpp = color ? 4 : 1, rowBytes = width * (color ? 4 : 1)
        var pixels = [UInt8](repeating: 0, count: rowBytes * height)
        try pixels.withUnsafeMutableBytes { data in
            let space = color ? CGColorSpace(name: CGColorSpace.sRGB)! : CGColorSpaceCreateDeviceGray()
            let info = color ? CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue : CGImageAlphaInfo.none.rawValue
            guard let context = CGContext(data: data.baseAddress, width: width, height: height,
                bitsPerComponent: 8, bytesPerRow: rowBytes, space: space, bitmapInfo: info)
            else { throw TerminalRenderError.unavailable }
            context.setAllowsFontSmoothing(false); context.setShouldSmoothFonts(false)
            context.setAllowsAntialiasing(true); context.setShouldAntialias(true)
            context.setFillColor(gray: 1, alpha: 1)
            var point = CGPoint(x: -left, y: -bottom)
            CTFontDrawGlyphs(font, &glyph, &point, 1, context)
        }
        // Core Graphics user space increases upward, but a bitmap context stores
        // its TOP row first, so `pixels` is already in the atlas's downward row
        // order (the canonical text shader's offset contract). Flipping it here
        // drew every glyph upside down (observed in the first Metal render).
        let position = try color ? colorShelf.reserve(width: width, height: height) : grayShelf.reserve(width: width, height: height)
        let texture = color ? resources.color : resources.gray
        pixels.withUnsafeBytes { bytes in
            texture.replace(region: MTLRegionMake2D(position.0, position.1, width, height), mipmapLevel: 0,
                withBytes: bytes.baseAddress!, bytesPerRow: width * bpp)
        }
        let placed = PlacedGlyph(color: color, x: position.0, y: position.1, width: width, height: height, left: left, top: top)
        glyphs[key] = placed
        return placed
    }
}
#endif

package com.sharescreen.android.webrtc

import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjection
import android.util.Log
import kotlinx.coroutines.suspendCancellableCoroutine
import org.webrtc.DefaultVideoDecoderFactory
import org.webrtc.DefaultVideoEncoderFactory
import org.webrtc.EglBase
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.PeerConnection
import org.webrtc.PeerConnectionFactory
import org.webrtc.SdpObserver
import org.webrtc.ScreenCapturerAndroid
import org.webrtc.SessionDescription
import org.webrtc.SurfaceTextureHelper
import org.webrtc.VideoSource
import org.webrtc.VideoTrack
import org.webrtc.audio.JavaAudioDeviceModule
import java.util.concurrent.ConcurrentHashMap
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/**
 * Manages the WebRTC stack: PeerConnectionFactory, local tracks (mic + screen),
 * and screen capture via MediaProjection.
 */
class WebRtcManager(private val context: Context) {

    companion object {
        private const val TAG = "WebRtcManager"

        /** Longest side of the shared-screen capture; keeps bandwidth and encoder cost sane. */
        private const val MAX_CAPTURE_SIDE = 1920

        /**
         * Capture size for the current display, preserving its aspect ratio.
         *
         * A fixed 16:9 request makes MediaProjection stretch or letterbox a portrait
         * phone screen, which shows up as a cut/squashed share on the other side.
         * Dimensions stay even because some encoders reject odd sizes.
         */
        fun captureSizeFor(widthPx: Int, heightPx: Int, maxSide: Int = MAX_CAPTURE_SIDE): Pair<Int, Int> {
            if (widthPx <= 0 || heightPx <= 0) return 1280 to 720
            val scale = minOf(1f, maxSide.toFloat() / maxOf(widthPx, heightPx))
            val w = ((widthPx * scale).toInt() / 2) * 2
            val h = ((heightPx * scale).toInt() / 2) * 2
            return maxOf(w, 2) to maxOf(h, 2)
        }

        private var factoryInitialized = false
    }

    private val eglBase: EglBase = EglBase.create()

    val eglBaseContext: EglBase.Context
        get() = eglBase.eglBaseContext

    private val audioDeviceModule: JavaAudioDeviceModule =
        JavaAudioDeviceModule.builder(context)
            .setUseHardwareAcousticEchoCanceler(true)
            .setUseHardwareNoiseSuppressor(true)
            .createAudioDeviceModule()

    private val factory: PeerConnectionFactory

    private val audioSource = lazy { factory.createAudioSource(MediaConstraints()) }
    val audioTrack = lazy {
        factory.createAudioTrack("ARDAMSa0", audioSource.value).apply {
            setEnabled(false)
        }
    }

    private var videoSource: VideoSource? = null
    var videoTrack: VideoTrack? = null
        private set

    private var screenCapturer: ScreenCapturerAndroid? = null
    private var surfaceTextureHelper: SurfaceTextureHelper? = null

    val remoteVideoTracks = ConcurrentHashMap<String, VideoTrack>()

    init {
        if (!factoryInitialized) {
            PeerConnectionFactory.initialize(
                PeerConnectionFactory.InitializationOptions.builder(context)
                    .setEnableInternalTracer(true)
                    .createInitializationOptions()
            )
            factoryInitialized = true
        }

        val encoderFactory = DefaultVideoEncoderFactory(eglBase.eglBaseContext, true, true)
        val decoderFactory = DefaultVideoDecoderFactory(eglBase.eglBaseContext)

        factory = PeerConnectionFactory.builder()
            .setAudioDeviceModule(audioDeviceModule)
            .setVideoEncoderFactory(encoderFactory)
            .setVideoDecoderFactory(decoderFactory)
            .createPeerConnectionFactory()

        videoSource = factory.createVideoSource(true)
        videoTrack = factory.createVideoTrack("ARDAMSv0", videoSource).apply {
            setEnabled(false)
        }
    }

    fun createPeerConnection(
        iceServers: List<PeerConnection.IceServer>,
        observer: PeerConnection.Observer,
    ): PeerConnection {
        val config = PeerConnection.RTCConfiguration(iceServers).apply {
            iceTransportsType = PeerConnection.IceTransportsType.ALL
            bundlePolicy = PeerConnection.BundlePolicy.MAXBUNDLE
            sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
            continualGatheringPolicy = PeerConnection.ContinualGatheringPolicy.GATHER_CONTINUALLY
        }
        return checkNotNull(factory.createPeerConnection(config, observer))
    }

    fun addLocalTracks(pc: PeerConnection) {
        pc.addTrack(audioTrack.value)
        pc.addTrack(videoTrack!!)
    }

    fun startScreenCapture(permissionIntent: Intent, onStopped: () -> Unit) {
        if (screenCapturer != null) return

        screenCapturer = ScreenCapturerAndroid(permissionIntent, object : MediaProjection.Callback() {
            override fun onStop() {
                Log.i(TAG, "MediaProjection stopped by system")
                onStopped()
                stopScreenCapture()
            }
        })

        surfaceTextureHelper = SurfaceTextureHelper.create("CaptureThread", eglBase.eglBaseContext)
        screenCapturer!!.initialize(surfaceTextureHelper, context, videoSource!!.capturerObserver)
        val (width, height) = captureSizeFor(
            context.resources.displayMetrics.widthPixels,
            context.resources.displayMetrics.heightPixels,
        )
        // shortcut: format fixed at start, so rotating mid-share keeps the old
        // orientation until sharing restarts. Upgrade with a display listener.
        screenCapturer!!.startCapture(width, height, 30)
        videoTrack!!.setEnabled(true)
        Log.i(TAG, "screen capture started at ${width}x$height")
    }

    fun stopScreenCapture() {
        try {
            screenCapturer?.stopCapture()
            screenCapturer?.dispose()
        } catch (e: Exception) {
            Log.w(TAG, "error stopping screen capture: ${e.message}")
        }
        surfaceTextureHelper?.dispose()
        screenCapturer = null
        surfaceTextureHelper = null
        videoTrack?.setEnabled(false)
    }

    fun setMicEnabled(enabled: Boolean) = audioTrack.value.setEnabled(enabled)
    fun isMicEnabled(): Boolean = audioTrack.value.enabled()

    fun storeRemoteVideoTrack(peerId: String, track: VideoTrack) {
        remoteVideoTracks[peerId] = track
    }

    fun removeRemoteVideoTrack(peerId: String) {
        remoteVideoTracks.remove(peerId)
    }

    fun addIceCandidate(pc: PeerConnection, data: com.sharescreen.android.signaling.IceCandidateData) {
        pc.addIceCandidate(IceCandidate(data.sdpMid, data.sdpMLineIndex, data.candidate))
    }

    fun iceCandidateToJson(candidate: IceCandidate): String {
        val json = org.json.JSONObject()
        json.put("candidate", candidate.sdp)
        json.put("sdpMid", candidate.sdpMid)
        json.put("sdpMLineIndex", candidate.sdpMLineIndex)
        return json.toString()
    }

    fun sdpToJson(sdp: SessionDescription): String {
        val json = org.json.JSONObject()
        json.put("type", sdp.type.canonicalForm())
        json.put("sdp", sdp.description)
        return json.toString()
    }

    fun parseSdp(jsonString: String): SessionDescription {
        val data = com.sharescreen.android.signaling.parseSdpDescription(jsonString)
        return SessionDescription(SessionDescription.Type.fromCanonicalForm(data.type), data.sdp)
    }

    fun dispose() {
        stopScreenCapture()
        audioSource.value.dispose()
        audioTrack.value.dispose()
        videoSource?.dispose()
        videoTrack?.dispose()
        audioDeviceModule.release()
        factory.dispose()
        eglBase.release()
        remoteVideoTracks.clear()
    }
}

// --- Top-level suspend wrappers for SDP operations ---

suspend fun PeerConnection.createOfferSuspend(constraints: MediaConstraints): SessionDescription =
    suspendCancellableCoroutine { cont ->
        createOffer(object : SdpObserver {
            override fun onCreateSuccess(sdp: SessionDescription?) {
                if (sdp != null) cont.resume(sdp)
                else cont.resumeWithException(IllegalStateException("null SDP"))
            }
            override fun onSetSuccess() {}
            override fun onCreateFailure(error: String?) {
                cont.resumeWithException(RuntimeException("createOffer: $error"))
            }
            override fun onSetFailure(error: String?) {}
        }, constraints)
    }

suspend fun PeerConnection.createAnswerSuspend(constraints: MediaConstraints): SessionDescription =
    suspendCancellableCoroutine { cont ->
        createAnswer(object : SdpObserver {
            override fun onCreateSuccess(sdp: SessionDescription?) {
                if (sdp != null) cont.resume(sdp)
                else cont.resumeWithException(IllegalStateException("null SDP"))
            }
            override fun onSetSuccess() {}
            override fun onCreateFailure(error: String?) {
                cont.resumeWithException(RuntimeException("createAnswer: $error"))
            }
            override fun onSetFailure(error: String?) {}
        }, constraints)
    }

suspend fun PeerConnection.setLocalDescriptionSuspend(sdp: SessionDescription) {
    suspendCancellableCoroutine { cont ->
        setLocalDescription(object : SdpObserver {
            override fun onCreateSuccess(p0: SessionDescription?) {}
            override fun onCreateFailure(p0: String?) {}
            override fun onSetSuccess() { cont.resume(Unit) }
            override fun onSetFailure(error: String?) {
                cont.resumeWithException(RuntimeException("setLocalDescription: $error"))
            }
        }, sdp)
    }
}

suspend fun PeerConnection.setRemoteDescriptionSuspend(sdp: SessionDescription) {
    suspendCancellableCoroutine { cont ->
        setRemoteDescription(object : SdpObserver {
            override fun onCreateSuccess(p0: SessionDescription?) {}
            override fun onCreateFailure(p0: String?) {}
            override fun onSetSuccess() { cont.resume(Unit) }
            override fun onSetFailure(error: String?) {
                cont.resumeWithException(RuntimeException("setRemoteDescription: $error"))
            }
        }, sdp)
    }
}

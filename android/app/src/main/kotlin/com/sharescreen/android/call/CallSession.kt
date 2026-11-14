package com.sharescreen.android.call

import android.content.Context
import android.content.Intent
import android.util.Log
import com.sharescreen.android.signaling.IceCandidateData
import com.sharescreen.android.signaling.PeerInfo
import com.sharescreen.android.signaling.PeerPayload
import com.sharescreen.android.signaling.SignalMessage
import com.sharescreen.android.signaling.SignalingClient
import com.sharescreen.android.signaling.parseIceCandidate
import com.sharescreen.android.webrtc.ScreenCaptureService
import com.sharescreen.android.webrtc.WebRtcManager
import com.sharescreen.android.webrtc.createAnswerSuspend
import com.sharescreen.android.webrtc.createOfferSuspend
import com.sharescreen.android.webrtc.setLocalDescriptionSuspend
import com.sharescreen.android.webrtc.setRemoteDescriptionSuspend
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import org.webrtc.DataChannel
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.PeerConnection
import org.webrtc.RtpReceiver
import org.webrtc.RtpTransceiver
import org.webrtc.VideoTrack
import java.util.concurrent.ConcurrentHashMap

/**
 * Orchestrates a WebRTC mesh call: one peer connection per remote
 * participant, with SDP/ICE relayed through the signaling server.
 *
 * Mirrors the flow in crates/call/src/lib.rs:
 * - Existing peers offer to newcomers (PeerJoined → offer).
 * - Newcomers answer offers (FromPeer(Offer) → answer).
 * - ICE candidates are buffered until the remote description is set.
 * - MediaState is broadcast on join and on toggle.
 */
class CallSession(
    private val context: Context,
    private val selfId: String,
    private val selfName: String,
    private val signalingUrl: String,
    private val initialIceServers: List<PeerConnection.IceServer>,
) {
    companion object {
        private const val TAG = "CallSession"
    }

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val signaling = SignalingClient()
    private lateinit var webRtc: WebRtcManager

    private val peers = ConcurrentHashMap<String, PeerConnection>()
    private val pendingIce = ConcurrentHashMap<String, MutableList<IceCandidateData>>()
    private val iceServers = mutableListOf<PeerConnection.IceServer>()
    private val roster = ConcurrentHashMap<String, Participant>()

    private var roomId: String? = null
    private var micOn = false
    private var screenOn = false

    // Expose the WebRtcManager for video rendering in the UI.
    val webrtc: WebRtcManager
        get() = webRtc

    private val _events = MutableSharedFlow<CallEvent>(extraBufferCapacity = 64)
    val events: SharedFlow<CallEvent> = _events.asSharedFlow()

    private val _connected = MutableStateFlow(false)
    val connected: StateFlow<Boolean> = _connected.asStateFlow()

    private val sdpConstraints = MediaConstraints().apply {
        mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveAudio", "true"))
        mandatory.add(MediaConstraints.KeyValuePair("OfferToReceiveVideo", "true"))
    }

    fun start() {
        iceServers.addAll(initialIceServers)
        webRtc = WebRtcManager(context)

        signaling.connect(signalingUrl, selfId, selfName)

        scope.launch {
            signaling.connectionState.collect { state ->
                when (state) {
                    SignalingClient.ConnectionState.Connected -> {
                        Log.i(TAG, "signaling connected")
                        _connected.value = true
                        _events.tryEmit(CallEvent.SignalingConnected)
                        signaling.send(SignalMessage.GetTurnCredentials)
                    }
                    is SignalingClient.ConnectionState.Disconnected -> {
                        Log.w(TAG, "signaling disconnected: ${state.reason}")
                        _connected.value = false
                        _events.tryEmit(CallEvent.SignalingDisconnected(state.reason))
                    }
                    else -> {}
                }
            }
        }

        scope.launch {
            signaling.incoming.collect { msg ->
                try {
                    handleMessage(msg)
                } catch (e: Exception) {
                    Log.e(TAG, "error handling ${msg::class.simpleName}: ${e.message}", e)
                    _events.tryEmit(CallEvent.Error(e.message ?: "unknown error"))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Room operations
    // -----------------------------------------------------------------------

    fun createRoom() {
        signaling.send(SignalMessage.CreateRoom)
    }

    fun joinRoom(id: String) {
        signaling.send(SignalMessage.JoinRoom(id))
    }

    fun leaveRoom() {
        signaling.send(SignalMessage.LeaveRoom)
        closeAllPeers()
    }

    fun deleteRoom() {
        signaling.send(SignalMessage.DeleteRoom)
        closeAllPeers()
    }

    // -----------------------------------------------------------------------
    // Media toggles
    // -----------------------------------------------------------------------

    fun setMicOn(on: Boolean) {
        if (micOn == on) return
        micOn = on
        webRtc.setMicEnabled(on)
        broadcastMediaState()
    }

    fun isMicOn(): Boolean = micOn

    /**
     * Called by the UI after the user grants the MediaProjection permission.
     * Starts the foreground service, then screen capture once the service is
     * actually foreground: Android 14+ rejects MediaProjection before that.
     */
    fun onScreenCaptureGranted(permissionIntent: Intent) {
        ScreenCaptureService.start(context) {
            try {
                webRtc.startScreenCapture(permissionIntent) {
                    // Called when MediaProjection is stopped by the system.
                    scope.launch {
                        screenOn = false
                        ScreenCaptureService.stop(context)
                        broadcastMediaState()
                    }
                }
                screenOn = true
                broadcastMediaState()
            } catch (e: Exception) {
                Log.e(TAG, "screen capture failed to start: ${e.message}", e)
                screenOn = false
                ScreenCaptureService.stop(context)
                broadcastMediaState()
                _events.tryEmit(CallEvent.ScreenShareFailed("Screen share failed: ${e.message}"))
            }
        }
    }

    fun stopScreenShare() {
        webRtc.stopScreenCapture()
        ScreenCaptureService.stop(context)
        screenOn = false
        broadcastMediaState()
    }

    fun isScreenOn(): Boolean = screenOn

    // -----------------------------------------------------------------------
    // Signaling message handling
    // -----------------------------------------------------------------------

    private suspend fun handleMessage(msg: SignalMessage) {
        when (msg) {
            is SignalMessage.RoomCreated -> {
                roomId = msg.roomId
                _events.tryEmit(CallEvent.RoomCreated(msg.roomId))
            }

            is SignalMessage.Joined -> {
                roomId = msg.roomId
                for (peer in msg.peers) {
                    upsertParticipant(peer)
                    _events.tryEmit(CallEvent.ParticipantJoined(roster[peer.id]!!))
                    sendMediaState(peer.id)
                }
                _events.tryEmit(CallEvent.RoomJoined(msg.roomId))
            }

            is SignalMessage.RoomDeleted -> {
                roomId = null
                roster.clear()
                closeAllPeers()
                _events.tryEmit(CallEvent.RoomDeleted(msg.roomId))
            }

            is SignalMessage.PeerJoined -> {
                upsertParticipant(msg.peer)
                _events.tryEmit(CallEvent.ParticipantJoined(roster[msg.peer.id]!!))
                sendMediaState(msg.peer.id)
                // Existing peers offer to the newcomer.
                offerTo(msg.peer.id)
            }

            is SignalMessage.PeerLeft -> {
                roster.remove(msg.id)
                webRtc.removeRemoteVideoTrack(msg.id)
                closePeer(msg.id)
                _events.tryEmit(CallEvent.ParticipantLeft(msg.id))
            }

            is SignalMessage.FromPeer -> {
                handleFromPeer(msg.from, msg.payload)
            }

            is SignalMessage.TurnCredentials -> {
                iceServers.add(
                    PeerConnection.IceServer.builder(msg.urls)
                        .setUsername(msg.username)
                        .setPassword(msg.credential)
                        .createIceServer()
                )
                Log.i(TAG, "added TURN credentials, ${iceServers.size} ICE server(s)")
            }

            is SignalMessage.Error -> {
                Log.w(TAG, "server error: ${msg.message}")
                _events.tryEmit(CallEvent.Error(msg.message))
            }

            else -> {}
        }
    }

    private suspend fun handleFromPeer(from: String, payload: PeerPayload) {
        when (payload) {
            is PeerPayload.Offer -> {
                Log.i(TAG, "received offer from $from")
                answerPeer(from, payload.sdp)
            }

            is PeerPayload.Answer -> {
                Log.i(TAG, "received answer from $from")
                val pc = peers[from]
                if (pc != null) {
                    val answer = webRtc.parseSdp(payload.sdp)
                    pc.setRemoteDescriptionSuspend(answer)
                    flushPendingIce(from)
                }
            }

            is PeerPayload.IceCandidate -> {
                val candidate = parseIceCandidate(payload.candidate)
                addOrBufferCandidate(from, candidate)
            }

            is PeerPayload.MediaState -> {
                val updated = updateParticipant(from) { p ->
                    p.copy(micOn = payload.micOn, screenOn = payload.screenOn)
                }
                if (updated != null) {
                    _events.tryEmit(CallEvent.ParticipantUpdated(updated))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Peer connection management
    // -----------------------------------------------------------------------

    private suspend fun offerTo(peerId: String) {
        Log.i(TAG, "offering to $peerId")
        val pc = createPeer(peerId)
        webRtc.addLocalTracks(pc)

        val offer = pc.createOfferSuspend(sdpConstraints)
        pc.setLocalDescriptionSuspend(offer)
        signaling.send(SignalMessage.ToPeer(peerId, PeerPayload.Offer(webRtc.sdpToJson(offer))))
    }

    private suspend fun answerPeer(peerId: String, offerSdp: String) {
        Log.i(TAG, "answering $peerId")
        val pc = peers[peerId] ?: createPeer(peerId)
        webRtc.addLocalTracks(pc)

        val offer = webRtc.parseSdp(offerSdp)
        pc.setRemoteDescriptionSuspend(offer)
        flushPendingIce(peerId)

        val answer = pc.createAnswerSuspend(sdpConstraints)
        pc.setLocalDescriptionSuspend(answer)
        signaling.send(SignalMessage.ToPeer(peerId, PeerPayload.Answer(webRtc.sdpToJson(answer))))
    }

    private fun createPeer(peerId: String): PeerConnection {
        val pc = webRtc.createPeerConnection(iceServers.toList(), createObserver(peerId))
        peers[peerId] = pc
        return pc
    }

    private fun createObserver(peerId: String): PeerConnection.Observer =
        object : PeerConnection.Observer {
            override fun onIceCandidate(candidate: IceCandidate?) {
                val c = candidate ?: return
                val json = webRtc.iceCandidateToJson(c)
                scope.launch {
                    signaling.send(SignalMessage.ToPeer(peerId, PeerPayload.IceCandidate(json)))
                }
            }

            override fun onConnectionChange(state: PeerConnection.PeerConnectionState?) {
                val s = state?.toCallState() ?: return
                Log.i(TAG, "peer $peerId connection: $s")
                val updated = updateParticipant(peerId) { it.copy(connection = s) }
                if (updated != null) {
                    _events.tryEmit(CallEvent.ParticipantUpdated(updated))
                }
                _events.tryEmit(CallEvent.ConnectionStateChanged(peerId, s))
            }

            override fun onAddTrack(receiver: RtpReceiver?, streams: Array<out MediaStream>?) {
                val track = receiver?.track() ?: return
                if (track is VideoTrack) {
                    Log.i(TAG, "remote video track from $peerId")
                    webRtc.storeRemoteVideoTrack(peerId, track)
                }
            }

            // Required overrides — no-ops for our use case.
            override fun onSignalingChange(state: PeerConnection.SignalingState) {}
            override fun onIceConnectionChange(state: PeerConnection.IceConnectionState) {}
            override fun onIceConnectionReceivingChange(receiving: Boolean) {}
            override fun onIceGatheringChange(state: PeerConnection.IceGatheringState) {}
            override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>) {}
            override fun onAddStream(stream: MediaStream) {}
            override fun onRemoveStream(stream: MediaStream) {}
            override fun onDataChannel(channel: DataChannel) {}
            override fun onRenegotiationNeeded() {}
            override fun onTrack(transceiver: RtpTransceiver) {}
        }

    private suspend fun addOrBufferCandidate(peerId: String, candidate: IceCandidateData) {
        val pc = peers[peerId]
        if (pc != null) {
            try {
                webRtc.addIceCandidate(pc, candidate)
                return
            } catch (e: Exception) {
                Log.w(TAG, "failed to add ICE candidate for $peerId: ${e.message}")
            }
        }
        pendingIce.getOrPut(peerId) { mutableListOf() }.add(candidate)
        Log.d(TAG, "buffered ICE candidate for $peerId")
    }

    private fun flushPendingIce(peerId: String) {
        val pending = pendingIce.remove(peerId) ?: return
        val pc = peers[peerId] ?: return
        for (candidate in pending) {
            try {
                webRtc.addIceCandidate(pc, candidate)
            } catch (e: Exception) {
                Log.w(TAG, "failed to flush ICE candidate for $peerId: ${e.message}")
            }
        }
        Log.d(TAG, "flushed ${pending.size} buffered ICE candidate(s) for $peerId")
    }

    private fun closePeer(peerId: String) {
        webRtc.removeRemoteVideoTrack(peerId)
        pendingIce.remove(peerId)
        peers.remove(peerId)?.close()
    }

    private fun closeAllPeers() {
        for (peerId in peers.keys.toList()) {
            closePeer(peerId)
        }
        roster.clear()
        pendingIce.clear()
    }

    // -----------------------------------------------------------------------
    // Participant helpers
    // -----------------------------------------------------------------------

    private fun upsertParticipant(peer: PeerInfo) {
        roster[peer.id] = Participant(
            id = peer.id,
            name = peer.name,
            micOn = roster[peer.id]?.micOn ?: false,
            screenOn = roster[peer.id]?.screenOn ?: false,
            connection = roster[peer.id]?.connection ?: ConnectionState.New,
        )
    }

    private fun updateParticipant(peerId: String, change: (Participant) -> Participant): Participant? {
        val current = roster[peerId] ?: return null
        val updated = change(current)
        roster[peerId] = updated
        return updated
    }

    // -----------------------------------------------------------------------
    // Media state broadcast
    // -----------------------------------------------------------------------

    private fun sendMediaState(peerId: String) {
        signaling.send(
            SignalMessage.ToPeer(peerId, PeerPayload.MediaState(micOn = micOn, screenOn = screenOn))
        )
    }

    private fun broadcastMediaState() {
        for (peerId in roster.keys) {
            sendMediaState(peerId)
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    fun disconnect() {
        if (roomId != null) {
            signaling.send(SignalMessage.LeaveRoom)
        }
        signaling.close()
        screenOn = false
        ScreenCaptureService.stop(context)
        closeAllPeers()
        webRtc.dispose()
    }
}

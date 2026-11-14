package com.sharescreen.android.call

/**
 * Events emitted by [CallSession] and consumed by the UI via [CallViewModel].
 *
 * Mirrors `CallEvent` in crates/core/src/call.rs.
 */
sealed class CallEvent {
    data class RoomCreated(val roomId: String) : CallEvent()
    data class RoomJoined(val roomId: String) : CallEvent()
    data class RoomDeleted(val roomId: String) : CallEvent()
    data class ParticipantJoined(val participant: Participant) : CallEvent()
    data class ParticipantLeft(val id: String) : CallEvent()
    data class ParticipantUpdated(val participant: Participant) : CallEvent()
    data class ConnectionStateChanged(val participantId: String, val state: ConnectionState) : CallEvent()
    data class Error(val message: String) : CallEvent()
    data class ScreenShareFailed(val message: String) : CallEvent()
    data object SignalingConnected : CallEvent()
    data class SignalingDisconnected(val reason: String? = null) : CallEvent()
}

data class Participant(
    val id: String,
    val name: String,
    val micOn: Boolean = false,
    val screenOn: Boolean = false,
    val connection: ConnectionState = ConnectionState.New,
)

enum class ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

fun org.webrtc.PeerConnection.PeerConnectionState.toCallState(): ConnectionState = when (this) {
    org.webrtc.PeerConnection.PeerConnectionState.NEW -> ConnectionState.New
    org.webrtc.PeerConnection.PeerConnectionState.CONNECTING -> ConnectionState.Connecting
    org.webrtc.PeerConnection.PeerConnectionState.CONNECTED -> ConnectionState.Connected
    org.webrtc.PeerConnection.PeerConnectionState.DISCONNECTED -> ConnectionState.Disconnected
    org.webrtc.PeerConnection.PeerConnectionState.FAILED -> ConnectionState.Failed
    org.webrtc.PeerConnection.PeerConnectionState.CLOSED -> ConnectionState.Closed
    else -> ConnectionState.Disconnected
}

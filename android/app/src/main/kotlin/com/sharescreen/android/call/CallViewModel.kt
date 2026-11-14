package com.sharescreen.android.call

import android.app.Application
import android.content.Intent
import android.util.Log
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.sharescreen.android.webrtc.WebRtcManager
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import org.webrtc.EglBase
import org.webrtc.PeerConnection

enum class AppView { Home, Call }

data class CallUiState(
    val view: AppView = AppView.Home,
    val connected: Boolean = false,
    val connecting: Boolean = false,
    val selfName: String = "",
    val serverUrl: String = "ws://10.0.2.2:3000/ws",
    val roomIdInput: String = "",
    val currentRoomId: String? = null,
    val participants: List<Participant> = emptyList(),
    val micOn: Boolean = false,
    val screenOn: Boolean = false,
    val error: String? = null,
)

class CallViewModel(application: Application) : AndroidViewModel(application) {

    companion object {
        private const val TAG = "CallViewModel"
    }

    private val context = application.applicationContext
    private var callSession: CallSession? = null

    private val _uiState = MutableStateFlow(CallUiState())
    val uiState: StateFlow<CallUiState> = _uiState.asStateFlow()

    /** Emitted when the UI should launch the MediaProjection permission dialog. */
    private val _requestScreenCapture = MutableSharedFlow<Unit>(extraBufferCapacity = 1)
    val requestScreenCapture: SharedFlow<Unit> = _requestScreenCapture.asSharedFlow()

    /** Expose the WebRtcManager for video rendering (EglBase context, remote tracks). */
    val webrtc: WebRtcManager?
        get() = callSession?.webrtc

    // --- Input setters ---

    fun updateName(value: String) = _uiState.update { it.copy(selfName = value) }
    fun updateServerUrl(value: String) = _uiState.update { it.copy(serverUrl = value) }
    fun updateRoomIdInput(value: String) = _uiState.update { it.copy(roomIdInput = value) }
    fun clearError() = _uiState.update { it.copy(error = null) }

    // --- Connection ---

    fun connect(name: String, serverUrl: String) {
        if (name.isBlank()) {
            _uiState.update { it.copy(error = "Enter your name") }
            return
        }

        _uiState.update { it.copy(connecting = true, selfName = name, serverUrl = serverUrl) }

        val peerId = "android-${System.currentTimeMillis().toString(16)}-${counter++}"
        val iceServers = listOf(
            PeerConnection.IceServer.builder("stun:stun.l.google.com:19302")
                .createIceServer()
        )

        callSession = CallSession(context, peerId, name, serverUrl, iceServers)
        callSession!!.start()

        viewModelScope.launch {
            callSession!!.events.collect { event -> handleEvent(event) }
        }
    }

    // --- Room actions ---

    fun createRoom() = callSession?.createRoom()

    fun joinRoom() {
        val roomId = _uiState.value.roomIdInput.trim()
        if (roomId.isNotBlank()) callSession?.joinRoom(roomId)
    }

    fun leaveRoom() {
        callSession?.leaveRoom()
        _uiState.update {
            it.copy(
                view = AppView.Home,
                currentRoomId = null,
                participants = emptyList(),
                micOn = false,
                screenOn = false,
            )
        }
    }

    fun deleteRoom() {
        callSession?.deleteRoom()
        _uiState.update {
            it.copy(
                view = AppView.Home,
                currentRoomId = null,
                participants = emptyList(),
                micOn = false,
                screenOn = false,
            )
        }
    }

    // --- Media toggles ---

    fun toggleMic() {
        val newMic = !_uiState.value.micOn
        callSession?.setMicOn(newMic)
        _uiState.update { it.copy(micOn = newMic) }
    }

    fun requestScreenShare() {
        _requestScreenCapture.tryEmit(Unit)
    }

    fun onScreenCaptureResult(intent: Intent) {
        callSession?.onScreenCaptureGranted(intent)
        _uiState.update { it.copy(screenOn = true) }
    }

    fun stopScreenShare() {
        callSession?.stopScreenShare()
        _uiState.update { it.copy(screenOn = false) }
    }

    // --- Event handling ---

    private fun handleEvent(event: CallEvent) {
        when (event) {
            is CallEvent.SignalingConnected -> {
                _uiState.update { it.copy(connected = true, connecting = false, error = null) }
            }

            is CallEvent.SignalingDisconnected -> {
                _uiState.update {
                    it.copy(
                        connected = false,
                        connecting = false,
                        view = AppView.Home,
                        currentRoomId = null,
                        participants = emptyList(),
                        micOn = false,
                        screenOn = false,
                        error = event.reason ?: "Connection lost",
                    )
                }
            }

            is CallEvent.RoomCreated -> {
                _uiState.update { it.copy(currentRoomId = event.roomId, view = AppView.Call) }
            }

            is CallEvent.RoomJoined -> {
                _uiState.update { it.copy(currentRoomId = event.roomId, view = AppView.Call) }
            }

            is CallEvent.RoomDeleted -> {
                _uiState.update {
                    it.copy(
                        view = AppView.Home,
                        currentRoomId = null,
                        participants = emptyList(),
                        micOn = false,
                        screenOn = false,
                    )
                }
            }

            is CallEvent.ParticipantJoined -> {
                _uiState.update {
                    it.copy(participants = it.participants.filterNot { p -> p.id == event.participant.id } + event.participant)
                }
            }

            is CallEvent.ParticipantLeft -> {
                _uiState.update {
                    it.copy(participants = it.participants.filterNot { p -> p.id == event.id })
                }
            }

            is CallEvent.ParticipantUpdated -> {
                _uiState.update {
                    it.copy(
                        participants = it.participants.map { p ->
                            if (p.id == event.participant.id) event.participant else p
                        }
                    )
                }
            }

            is CallEvent.ConnectionStateChanged -> {
                // Already handled via ParticipantUpdated in most cases.
            }

            is CallEvent.Error -> {
                Log.w(TAG, "error: ${event.message}")
                _uiState.update { it.copy(error = event.message) }
            }

            is CallEvent.ScreenShareFailed -> {
                Log.w(TAG, "screen share failed: ${event.message}")
                _uiState.update { it.copy(screenOn = false, error = event.message) }
            }
        }
    }

    override fun onCleared() {
        callSession?.disconnect()
        callSession = null
    }

    private var counter = 0
}

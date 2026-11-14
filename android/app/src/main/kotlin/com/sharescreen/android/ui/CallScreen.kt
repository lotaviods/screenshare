package com.sharescreen.android.ui

import android.app.Activity
import android.content.Context
import android.media.projection.MediaProjectionManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.rememberTransformableState
import androidx.compose.foundation.gestures.transformable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CallEnd
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material.icons.filled.ScreenShare
import androidx.compose.material.icons.filled.StopScreenShare
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.sharescreen.android.call.CallViewModel
import com.sharescreen.android.call.ConnectionState
import com.sharescreen.android.call.Participant
import com.sharescreen.android.webrtc.TextureVideoRenderer
import org.webrtc.EglBase
import org.webrtc.VideoTrack

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun CallScreen(viewModel: CallViewModel) {
    val uiState by viewModel.uiState.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val snackbarHostState = remember { SnackbarHostState() }

    val mediaProjectionManager = remember {
        context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
    }

    val screenCaptureLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == Activity.RESULT_OK && result.data != null) {
            viewModel.onScreenCaptureResult(result.data!!)
        }
    }

    LaunchedEffect(Unit) {
        viewModel.requestScreenCapture.collect {
            screenCaptureLauncher.launch(mediaProjectionManager.createScreenCaptureIntent())
        }
    }

    LaunchedEffect(uiState.error) {
        uiState.error?.let {
            snackbarHostState.showSnackbar(it)
            viewModel.clearError()
        }
    }

    val webrtc = viewModel.webrtc
    val eglBaseContext = webrtc?.eglBaseContext

    // Find who is sharing (first participant with screenOn, or self).
    val sharingPeer = uiState.participants.find { it.screenOn }
    val remoteTrack = sharingPeer?.let { webrtc?.remoteVideoTracks?.get(it.id) }
    val localTrack = if (uiState.screenOn) webrtc?.videoTrack else null
    val hasVideo = remoteTrack != null || localTrack != null

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Text("Room: ${uiState.currentRoomId ?: "—"}")
                },
                actions = {
                    IconButton(onClick = { viewModel.deleteRoom() }) {
                        Icon(Icons.Default.Delete, contentDescription = "Delete room")
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = Color.Black.copy(alpha = 0.8f),
                    titleContentColor = Color.White,
                    actionIconContentColor = Color.White,
                ),
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .background(Color.Black)
                .padding(padding),
        ) {
            // Video area
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth()
                    .clipToBounds(),
                contentAlignment = Alignment.Center,
            ) {
                if (eglBaseContext != null && remoteTrack != null) {
                    WebRtcVideo(remoteTrack, eglBaseContext, Modifier.fillMaxSize())
                } else if (eglBaseContext != null && localTrack != null && uiState.screenOn) {
                    // Show local screen if we're sharing and no remote share.
                    WebRtcVideo(localTrack, eglBaseContext, Modifier.fillMaxSize())
                } else {
                    Text(
                        text = if (hasVideo) "Connecting video…" else "No one is sharing",
                        color = Color.White.copy(alpha = 0.6f),
                        style = MaterialTheme.typography.bodyLarge,
                    )
                }

                // Local preview PiP (when user is sharing and remote is also showing).
                if (eglBaseContext != null && localTrack != null && remoteTrack != null && uiState.screenOn) {
                    Box(
                        modifier = Modifier
                            .align(Alignment.BottomEnd)
                            .padding(12.dp)
                            .size(width = 120.dp, height = 68.dp)
                            .clip(RoundedCornerShape(8.dp))
                            .background(Color.Black),
                    ) {
                        WebRtcVideo(localTrack, eglBaseContext, Modifier.fillMaxSize())
                    }
                }
            }

            // Participants row
            ParticipantsBar(
                participants = uiState.participants,
                selfName = uiState.selfName,
                selfMicOn = uiState.micOn,
                selfScreenOn = uiState.screenOn,
            )

            // Controls
            ControlsBar(
                micOn = uiState.micOn,
                screenOn = uiState.screenOn,
                onToggleMic = { viewModel.toggleMic() },
                onToggleScreen = {
                    if (uiState.screenOn) viewModel.stopScreenShare()
                    else viewModel.requestScreenShare()
                },
                onLeave = { viewModel.leaveRoom() },
            )
        }
    }
}

@Composable
private fun ParticipantsBar(
    participants: List<Participant>,
    selfName: String,
    selfMicOn: Boolean,
    selfScreenOn: Boolean,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(Color.Black.copy(alpha = 0.7f))
            .padding(horizontal = 12.dp, vertical = 8.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        // Self
        ParticipantChip(selfName, selfMicOn, selfScreenOn, ConnectionState.Connected)
        // Remote participants
        for (p in participants) {
            ParticipantChip(p.name, p.micOn, p.screenOn, p.connection)
        }
    }
}

@Composable
private fun ParticipantChip(
    name: String,
    micOn: Boolean,
    screenOn: Boolean,
    connection: ConnectionState,
) {
    Row(
        modifier = Modifier
            .clip(RoundedCornerShape(16.dp))
            .background(MaterialTheme.colorScheme.primaryContainer.copy(alpha = 0.3f))
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = name.take(12),
            style = MaterialTheme.typography.labelMedium,
            color = Color.White,
        )
        Spacer(Modifier.width(4.dp))
        Icon(
            imageVector = if (micOn) Icons.Default.Mic else Icons.Default.MicOff,
            contentDescription = null,
            modifier = Modifier.size(14.dp),
            tint = if (micOn) Color.White else Color.White.copy(alpha = 0.4f),
        )
        if (screenOn) {
            Spacer(Modifier.width(2.dp))
            Icon(
                imageVector = Icons.Default.ScreenShare,
                contentDescription = null,
                modifier = Modifier.size(14.dp),
                tint = Color.White,
            )
        }
        if (connection == ConnectionState.Connecting) {
            Spacer(Modifier.width(2.dp))
            Text("•", color = Color.Yellow, style = MaterialTheme.typography.labelSmall)
        }
    }
}

@Composable
private fun ControlsBar(
    micOn: Boolean,
    screenOn: Boolean,
    onToggleMic: () -> Unit,
    onToggleScreen: () -> Unit,
    onLeave: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(Color.Black)
            .padding(vertical = 12.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // Mic toggle
        FilledTonalButton(
            onClick = onToggleMic,
            modifier = Modifier.padding(horizontal = 4.dp),
        ) {
            Icon(
                if (micOn) Icons.Default.Mic else Icons.Default.MicOff,
                contentDescription = if (micOn) "Mute" else "Unmute",
            )
            Spacer(Modifier.width(6.dp))
            Text(if (micOn) "Mute" else "Unmute")
        }

        // Screen share toggle
        FilledTonalButton(
            onClick = onToggleScreen,
            modifier = Modifier.padding(horizontal = 4.dp),
        ) {
            Icon(
                if (screenOn) Icons.Default.StopScreenShare else Icons.Default.ScreenShare,
                contentDescription = if (screenOn) "Stop sharing" else "Share screen",
            )
            Spacer(Modifier.width(6.dp))
            Text(if (screenOn) "Stop" else "Share")
        }

        // Leave
        FilledTonalButton(
            onClick = onLeave,
            modifier = Modifier.padding(horizontal = 4.dp),
            colors = androidx.compose.material3.ButtonDefaults.filledTonalButtonColors(
                containerColor = MaterialTheme.colorScheme.error,
                contentColor = MaterialTheme.colorScheme.onError,
            ),
        ) {
            Icon(Icons.Default.CallEnd, contentDescription = "Leave")
            Spacer(Modifier.width(6.dp))
            Text("Leave")
        }
    }
}

/**
 * Compose wrapper for [TextureVideoRenderer].
 * Aspect-fit by default; pinch zooms (1x–4x) and pans, bounded to the frame.
 */
@Composable
fun WebRtcVideo(
    videoTrack: VideoTrack?,
    eglBaseContext: EglBase.Context,
    modifier: Modifier = Modifier,
) {
    var renderer by remember { mutableStateOf<TextureVideoRenderer?>(null) }
    var currentTrack by remember { mutableStateOf<VideoTrack?>(null) }
    var zoom by remember { mutableFloatStateOf(1f) }
    var pan by remember { mutableStateOf(Offset.Zero) }

    val gestureState = rememberTransformableState { zoomChange, panChange, _ ->
        val view = renderer ?: return@rememberTransformableState
        if (view.width == 0 || view.height == 0) return@rememberTransformableState
        zoom = (zoom * zoomChange).coerceIn(1f, 4f)
        val maxX = view.width * (zoom - 1f) / 2f
        val maxY = view.height * (zoom - 1f) / 2f
        pan = Offset(
            (pan.x + panChange.x).coerceIn(-maxX, maxX),
            (pan.y + panChange.y).coerceIn(-maxY, maxY),
        )
    }

    AndroidView(
        factory = { ctx ->
            TextureVideoRenderer(ctx, eglBaseContext).also { renderer = it }
        },
        onRelease = { view ->
            currentTrack?.let { track -> runCatching { track.removeSink(view) } }
            currentTrack = null
            view.release()
            renderer = null
        },
        update = { view ->
            view.scaleX = zoom
            view.scaleY = zoom
            view.translationX = pan.x
            view.translationY = pan.y
        },
        modifier = modifier
            .clipToBounds()
            .transformable(gestureState),
    )

    LaunchedEffect(videoTrack, renderer) {
        val r = renderer ?: return@LaunchedEffect
        currentTrack?.let { track -> runCatching { track.removeSink(r) } }
        currentTrack = videoTrack
        videoTrack?.addSink(r)
    }
}

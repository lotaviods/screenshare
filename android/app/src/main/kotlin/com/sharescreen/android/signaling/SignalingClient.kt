package com.sharescreen.android.signaling

import android.util.Log
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.util.concurrent.TimeUnit

/**
 * WebSocket signaling client matching the Rust signaling server.
 *
 * The server accepts WebSocket connections at `/ws?id=<peer_id>&name=<display_name>`
 * and relays JSON messages defined in [SignalMessage].
 */
class SignalingClient {

    companion object {
        private const val TAG = "SignalingClient"

        /**
         * Normalizes a user-typed server address into the endpoint the Rust server
         * serves: `<ws|wss>://host[:port]/ws?id=<peer_id>&name=<display_name>`.
         *
         * Accepts bare hosts (`example.com`), http(s) URLs, and ws(s) URLs, with or
         * without the `/ws` path. Returns null if the address cannot be parsed.
         */
        fun buildWebSocketUrl(baseUrl: String, peerId: String, name: String): String? {
            val trimmed = baseUrl.trim()
            if (trimmed.isEmpty()) return null

            val asHttp = when {
                trimmed.startsWith("ws://", ignoreCase = true) ->
                    "http://" + trimmed.substring(5)
                trimmed.startsWith("wss://", ignoreCase = true) ->
                    "https://" + trimmed.substring(6)
                trimmed.startsWith("http://", ignoreCase = true) ||
                    trimmed.startsWith("https://", ignoreCase = true) -> trimmed
                else -> "https://$trimmed"
            }
            val parsed: HttpUrl = asHttp.toHttpUrlOrNull() ?: return null

            // OkHttp's HttpUrl only models http(s); prepend the ws(s) scheme at the end.
            val httpUrl = parsed.newBuilder()
                .encodedPath(if (parsed.encodedPath.isBlank() || parsed.encodedPath == "/") "/ws" else parsed.encodedPath)
                .apply {
                    if (parsed.queryParameter("id") == null) addQueryParameter("id", peerId)
                    if (parsed.queryParameter("name") == null) addQueryParameter("name", name)
                }
                .build()
                .toString()

            val scheme = if (parsed.scheme == "http") "ws" else "wss"
            return scheme + "://" + httpUrl.substringAfter("://")
        }
    }

    private val client: OkHttpClient by lazy {
        OkHttpClient.Builder()
            .readTimeout(0, TimeUnit.SECONDS) // WebSocket stays open
            .pingInterval(15, TimeUnit.SECONDS)
            .build()
    }

    private var webSocket: WebSocket? = null

    /** True once [close] ran, so the resulting socket callback is not reported as a lost connection. */
    @Volatile
    private var closedByClient = false

    /** Flow of incoming signaling messages. */
    private val _incoming = MutableSharedFlow<SignalMessage>(extraBufferCapacity = 64)
    val incoming: SharedFlow<SignalMessage> = _incoming.asSharedFlow()

    private val _connectionState = MutableSharedFlow<ConnectionState>(replay = 1, extraBufferCapacity = 4)
    val connectionState: SharedFlow<ConnectionState> = _connectionState.asSharedFlow()

    /** Attempts to connect to the given server address. Returns false if the address is invalid. */
    fun connect(serverUrl: String, peerId: String, name: String): Boolean {
        val url = buildWebSocketUrl(serverUrl, peerId, name)
        if (url == null) {
            Log.e(TAG, "invalid signaling server address: $serverUrl")
            _connectionState.tryEmit(ConnectionState.Disconnected("Invalid server address: $serverUrl"))
            return false
        }

        _connectionState.tryEmit(ConnectionState.Connecting)
        closedByClient = false

        val request = Request.Builder().url(url).build()
        webSocket = client.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                Log.i(TAG, "WebSocket connected to $url")
                _connectionState.tryEmit(ConnectionState.Connected)
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                try {
                    val msg = parseSignalMessage(text)
                    _incoming.tryEmit(msg)
                } catch (e: Exception) {
                    Log.w(TAG, "ignoring malformed signaling message: ${e.message}")
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(code, reason)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                Log.i(TAG, "WebSocket closed: $code $reason")
                if (closedByClient) return
                _connectionState.tryEmit(
                    ConnectionState.Disconnected("Server closed the connection ($code ${reason.ifBlank { "no reason" }})")
                )
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                val detail = response?.let { "HTTP ${it.code}" } ?: t.message ?: t::class.simpleName
                Log.e(TAG, "WebSocket failure: $detail (url=$url)")
                if (closedByClient) return
                _connectionState.tryEmit(ConnectionState.Disconnected("Cannot reach $url: $detail"))
            }
        })
        return true
    }

    /** Send a signaling message. Returns false if the socket is not open. */
    fun send(message: SignalMessage): Boolean {
        val ws = webSocket ?: return false
        return ws.send(message.toJson())
    }

    /** Close the WebSocket connection. */
    fun close() {
        closedByClient = true
        webSocket?.close(1000, "client disconnect")
        webSocket = null
    }

    sealed interface ConnectionState {
        data object Connecting : ConnectionState
        data object Connected : ConnectionState

        /** [reason] explains the drop (unreachable host, HTTP status, close code). */
        data class Disconnected(val reason: String? = null) : ConnectionState
    }
}

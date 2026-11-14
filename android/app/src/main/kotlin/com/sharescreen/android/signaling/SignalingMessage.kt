package com.sharescreen.android.signaling

import org.json.JSONArray
import org.json.JSONObject

// ---------------------------------------------------------------------------
// Domain types matching crates/core/src/protocol.rs
// ---------------------------------------------------------------------------

data class PeerInfo(
    val id: String,
    val name: String,
)

/**
 * Media/peer payload relayed verbatim between two peers.
 *
 * `sdp` and `candidate` carry JSON-serialized SDP descriptions and ICE
 * candidates. The WebRTC layer owns (de)serialization of those inner blobs.
 *
 * Mirrors `PeerPayload` in protocol.rs, discriminated by "kind".
 */
sealed class PeerPayload {
    data class Offer(val sdp: String) : PeerPayload()
    data class Answer(val sdp: String) : PeerPayload()
    data class IceCandidate(val candidate: String) : PeerPayload()
    data class MediaState(val micOn: Boolean, val screenOn: Boolean) : PeerPayload()
}

/**
 * A signaling message exchanged over the WebSocket, both directions.
 *
 * Mirrors `SignalMessage` in protocol.rs, discriminated by "type".
 */
sealed class SignalMessage {
    // --- client → server ---
    data object CreateRoom : SignalMessage()
    data class JoinRoom(val roomId: String) : SignalMessage()
    data object LeaveRoom : SignalMessage()
    data object DeleteRoom : SignalMessage()
    data object GetTurnCredentials : SignalMessage()
    data class ToPeer(val to: String, val payload: PeerPayload) : SignalMessage()

    // --- server → client ---
    data class RoomCreated(val roomId: String) : SignalMessage()
    data class Joined(val roomId: String, val peers: List<PeerInfo>) : SignalMessage()
    data class RoomDeleted(val roomId: String) : SignalMessage()
    data class PeerJoined(val peer: PeerInfo) : SignalMessage()
    data class PeerLeft(val id: String) : SignalMessage()
    data class FromPeer(val from: String, val payload: PeerPayload) : SignalMessage()
    data class TurnCredentials(
        val urls: List<String>,
        val username: String,
        val credential: String,
    ) : SignalMessage()
    data class Error(val message: String) : SignalMessage()
}

// ---------------------------------------------------------------------------
// Serialization (matches serde with tag="type", rename_all="snake_case")
// ---------------------------------------------------------------------------

fun SignalMessage.toJson(): String = JSONObject().apply {
    when (this@toJson) {
        is SignalMessage.CreateRoom -> put("type", "create_room")
        is SignalMessage.JoinRoom -> {
            put("type", "join_room")
            put("room_id", roomId)
        }
        is SignalMessage.LeaveRoom -> put("type", "leave_room")
        is SignalMessage.DeleteRoom -> put("type", "delete_room")
        is SignalMessage.GetTurnCredentials -> put("type", "get_turn_credentials")
        is SignalMessage.ToPeer -> {
            put("type", "to_peer")
            put("to", to)
            put("payload", payload.toJson())
        }
        is SignalMessage.RoomCreated -> {
            put("type", "room_created")
            put("room_id", roomId)
        }
        is SignalMessage.Joined -> {
            put("type", "joined")
            put("room_id", roomId)
            put("peers", JSONArray().apply {
                peers.forEach { put(JSONObject().apply {
                    put("id", it.id)
                    put("name", it.name)
                })}
            })
        }
        is SignalMessage.RoomDeleted -> {
            put("type", "room_deleted")
            put("room_id", roomId)
        }
        is SignalMessage.PeerJoined -> {
            put("type", "peer_joined")
            put("peer", JSONObject().apply {
                put("id", peer.id)
                put("name", peer.name)
            })
        }
        is SignalMessage.PeerLeft -> {
            put("type", "peer_left")
            put("id", id)
        }
        is SignalMessage.FromPeer -> {
            put("type", "from_peer")
            put("from", from)
            put("payload", payload.toJson())
        }
        is SignalMessage.TurnCredentials -> {
            put("type", "turn_credentials")
            put("urls", JSONArray(urls))
            put("username", username)
            put("credential", credential)
        }
        is SignalMessage.Error -> {
            put("type", "error")
            put("message", message)
        }
    }
}.toString()

fun PeerPayload.toJson(): JSONObject = JSONObject().apply {
    when (this@toJson) {
        is PeerPayload.Offer -> {
            put("kind", "offer")
            put("sdp", sdp)
        }
        is PeerPayload.Answer -> {
            put("kind", "answer")
            put("sdp", sdp)
        }
        is PeerPayload.IceCandidate -> {
            put("kind", "ice_candidate")
            put("candidate", candidate)
        }
        is PeerPayload.MediaState -> {
            put("kind", "media_state")
            put("mic_on", micOn)
            put("screen_on", screenOn)
        }
    }
}

// ---------------------------------------------------------------------------
// Deserialization
// ---------------------------------------------------------------------------

fun parseSignalMessage(text: String): SignalMessage {
    val json = JSONObject(text)
    return when (json.getString("type")) {
        "create_room" -> SignalMessage.CreateRoom
        "join_room" -> SignalMessage.JoinRoom(json.getString("room_id"))
        "leave_room" -> SignalMessage.LeaveRoom
        "delete_room" -> SignalMessage.DeleteRoom
        "get_turn_credentials" -> SignalMessage.GetTurnCredentials
        "to_peer" -> SignalMessage.ToPeer(
            to = json.getString("to"),
            payload = parsePeerPayload(json.getJSONObject("payload")),
        )
        "room_created" -> SignalMessage.RoomCreated(json.getString("room_id"))
        "joined" -> {
            val peers = json.getJSONArray("peers")
            SignalMessage.Joined(
                roomId = json.getString("room_id"),
                peers = List(peers.length()) { i ->
                    val p = peers.getJSONObject(i)
                    PeerInfo(p.getString("id"), p.getString("name"))
                },
            )
        }
        "room_deleted" -> SignalMessage.RoomDeleted(json.getString("room_id"))
        "peer_joined" -> {
            val p = json.getJSONObject("peer")
            SignalMessage.PeerJoined(PeerInfo(p.getString("id"), p.getString("name")))
        }
        "peer_left" -> SignalMessage.PeerLeft(json.getString("id"))
        "from_peer" -> SignalMessage.FromPeer(
            from = json.getString("from"),
            payload = parsePeerPayload(json.getJSONObject("payload")),
        )
        "turn_credentials" -> {
            val urls = json.getJSONArray("urls")
            SignalMessage.TurnCredentials(
                urls = List(urls.length()) { urls.getString(it) },
                username = json.getString("username"),
                credential = json.getString("credential"),
            )
        }
        "error" -> SignalMessage.Error(json.getString("message"))
        else -> SignalMessage.Error("unknown message type: ${json.getString("type")}")
    }
}

fun parsePeerPayload(json: JSONObject): PeerPayload = when (json.getString("kind")) {
    "offer" -> PeerPayload.Offer(json.getString("sdp"))
    "answer" -> PeerPayload.Answer(json.getString("sdp"))
    "ice_candidate" -> PeerPayload.IceCandidate(json.getString("candidate"))
    "media_state" -> PeerPayload.MediaState(
        micOn = json.getBoolean("mic_on"),
        screenOn = json.getBoolean("screen_on"),
    )
    else -> throw IllegalArgumentException("unknown payload kind: ${json.getString("kind")}")
}

// ---------------------------------------------------------------------------
// SDP and ICE candidate (de)serialization matching crates/webrtc/src/peer.rs
//
// The desktop client stores SessionDescription as a JSON string
// {"type":"offer","sdp":"v=0..."} and IceCandidate as
// {"candidate":"...","sdpMid":"...","sdpMLineIndex":0,"usernameFragment":"..."}.
// ---------------------------------------------------------------------------

data class SdpDescription(val type: String, val sdp: String)

fun SdpDescription.toJsonString(): String = JSONObject()
    .put("type", type)
    .put("sdp", sdp)
    .toString()

fun parseSdpDescription(jsonString: String): SdpDescription {
    val json = JSONObject(jsonString)
    return SdpDescription(
        type = json.getString("type"),
        sdp = json.getString("sdp"),
    )
}

data class IceCandidateData(
    val candidate: String,
    val sdpMid: String?,
    val sdpMLineIndex: Int,
    val usernameFragment: String?,
)

fun IceCandidateData.toJsonString(): String = JSONObject().apply {
    put("candidate", candidate)
    if (sdpMid != null) put("sdpMid", sdpMid)
    if (sdpMLineIndex >= 0) put("sdpMLineIndex", sdpMLineIndex)
    if (usernameFragment != null) put("usernameFragment", usernameFragment)
}.toString()

fun parseIceCandidate(jsonString: String): IceCandidateData {
    val json = JSONObject(jsonString)
    return IceCandidateData(
        candidate = json.getString("candidate"),
        sdpMid = if (json.has("sdpMid") && !json.isNull("sdpMid")) json.getString("sdpMid") else null,
        sdpMLineIndex = if (json.has("sdpMLineIndex") && !json.isNull("sdpMLineIndex"))
            json.getInt("sdpMLineIndex") else -1,
        usernameFragment = if (json.has("usernameFragment") && !json.isNull("usernameFragment"))
            json.getString("usernameFragment") else null,
    )
}

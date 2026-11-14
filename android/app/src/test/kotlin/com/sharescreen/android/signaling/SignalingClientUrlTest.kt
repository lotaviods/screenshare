package com.sharescreen.android.signaling

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The server serves signaling only at `/ws?id=&name=`, so the client must build that URL. */
class SignalingClientUrlTest {

    @Test
    fun bareHostUsesSecureWebSocketAndWsPath() {
        assertEquals(
            "wss://signal.example.com/ws?id=p1&name=Ana",
            SignalingClient.buildWebSocketUrl("signal.example.com", "p1", "Ana"),
        )
    }

    @Test
    fun httpAndHttpsBecomeWsAndWss() {
        assertEquals(
            "ws://10.0.2.2:3000/ws?id=p1&name=Ana",
            SignalingClient.buildWebSocketUrl("http://10.0.2.2:3000", "p1", "Ana"),
        )
        assertEquals(
            "wss://signal.example.com/ws?id=p1&name=Ana",
            SignalingClient.buildWebSocketUrl("https://signal.example.com", "p1", "Ana"),
        )
    }

    @Test
    fun existingWsPathIsKept() {
        assertEquals(
            "wss://signal.example.com/ws?id=p1&name=Ana",
            SignalingClient.buildWebSocketUrl("wss://signal.example.com/ws", "p1", "Ana"),
        )
        assertEquals(
            "ws://10.0.2.2:3000/ws?id=p1&name=Ana",
            SignalingClient.buildWebSocketUrl("ws://10.0.2.2:3000/ws", "p1", "Ana"),
        )
    }

    @Test
    fun nameIsEncodedAndGivenQueryIsNotDuplicated() {
        assertEquals(
            "wss://h.example/ws?name=Ana%20Maria&id=custom",
            SignalingClient.buildWebSocketUrl("h.example?name=Ana%20Maria&id=custom", "p1", "Ana"),
        )
    }

    @Test
    fun invalidInputReturnsNull() {
        assertNull(SignalingClient.buildWebSocketUrl("", "p1", "Ana"))
        assertNull(SignalingClient.buildWebSocketUrl("   ", "p1", "Ana"))
        assertNull(SignalingClient.buildWebSocketUrl("not a url", "p1", "Ana"))
    }
}

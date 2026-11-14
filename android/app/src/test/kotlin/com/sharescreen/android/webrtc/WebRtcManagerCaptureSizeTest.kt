package com.sharescreen.android.webrtc

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class WebRtcManagerCaptureSizeTest {

    @Test
    fun portraitDisplayKeepsPortraitAspect() {
        val (w, h) = WebRtcManager.captureSizeFor(1080, 2340)
        assertTrue("longest side capped", maxOf(w, h) <= 1920)
        assertEquals(0, w % 2)
        assertEquals(0, h % 2)
        assertTrue("still portrait", h > w)
        assertEquals(1080f / 2340f, w.toFloat() / h, 0.01f)
    }

    @Test
    fun landscapeDisplayKeepsLandscapeAspect() {
        val (w, h) = WebRtcManager.captureSizeFor(2340, 1080)
        assertEquals(1920, w)
        assertEquals(0, h % 2)
        assertEquals(2340f / 1080f, w.toFloat() / h, 0.01f)
    }

    @Test
    fun smallDisplayIsNotUpscaled() {
        assertEquals(720 to 1280, WebRtcManager.captureSizeFor(720, 1280))
    }

    @Test
    fun invalidMetricsFallBackToDefault() {
        assertEquals(1280 to 720, WebRtcManager.captureSizeFor(0, 0))
    }
}

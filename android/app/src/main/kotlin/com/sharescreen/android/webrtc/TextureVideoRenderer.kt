package com.sharescreen.android.webrtc

import android.content.Context
import android.graphics.SurfaceTexture
import android.view.Surface
import android.view.TextureView
import org.webrtc.EglBase
import org.webrtc.GlRectDrawer
import org.webrtc.SurfaceEglRenderer
import org.webrtc.VideoFrame
import org.webrtc.VideoSink

/**
 * Minimal TextureView-based video renderer.
 *
 * `TextureViewRenderer` is absent from stream-webrtc-android, and a
 * `SurfaceViewRenderer` cannot be clipped or scaled reliably inside Compose.
 * This view draws frames aspect-fit inside the surface, so pinch zoom and
 * clipping behave like any other view.
 */
class TextureVideoRenderer(
    context: Context,
    eglBaseContext: EglBase.Context,
) : TextureView(context), TextureView.SurfaceTextureListener, VideoSink {

    private val renderer = SurfaceEglRenderer("TextureVideoRenderer")

    init {
        renderer.init(eglBaseContext, EglBase.CONFIG_PLAIN, GlRectDrawer())
        surfaceTextureListener = this
    }

    fun release() {
        renderer.release()
    }

    override fun onFrame(frame: VideoFrame) = renderer.onFrame(frame)

    override fun onSurfaceTextureAvailable(surfaceTexture: SurfaceTexture, width: Int, height: Int) {
        renderer.createEglSurface(Surface(surfaceTexture))
        updateLayoutAspect()
    }

    override fun onSurfaceTextureSizeChanged(surfaceTexture: SurfaceTexture, width: Int, height: Int) {
        updateLayoutAspect()
    }

    override fun onSurfaceTextureDestroyed(surfaceTexture: SurfaceTexture): Boolean {
        renderer.releaseEglSurface(null)
        return true
    }

    override fun onSurfaceTextureUpdated(surfaceTexture: SurfaceTexture) {}

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        updateLayoutAspect()
    }

    /** Layout aspect = view aspect, so EglRenderer letterboxes instead of stretching. */
    private fun updateLayoutAspect() {
        if (width > 0 && height > 0) {
            renderer.setLayoutAspectRatio(width.toFloat() / height)
        }
    }
}

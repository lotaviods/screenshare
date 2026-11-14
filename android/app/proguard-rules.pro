# Keep WebRTC native methods and JNI bridge classes
-keep class org.webrtc.** { *; }
-keepclassmembers class org.webrtc.** { *; }

# Keep Gson-serializable models
-keep class com.sharescreen.android.signaling.** { *; }
-keepclassmembers class com.sharescreen.android.signaling.** { *; }

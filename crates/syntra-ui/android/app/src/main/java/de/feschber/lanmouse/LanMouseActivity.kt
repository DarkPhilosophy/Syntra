package de.feschber.lanmouse

import android.app.NativeActivity
import android.os.Bundle

/** NativeActivity loads liblan_mouse_ui.so and dispatches android_main. */
class LanMouseActivity : NativeActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
    }
}

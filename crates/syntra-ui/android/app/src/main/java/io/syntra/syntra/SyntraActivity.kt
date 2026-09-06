package io.syntra.syntra

import android.app.NativeActivity
import android.os.Bundle

/** NativeActivity loads libsyntra_ui.so and dispatches android_main. */
class SyntraActivity : NativeActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
    }
}

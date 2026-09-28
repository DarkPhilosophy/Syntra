package io.syntra.syntra

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import java.io.File

/**
 * Lets the native app pick files through the system document picker.
 *
 * The UI runs in a NativeActivity, which cannot receive activity results, so
 * this transparent activity opens the picker, copies the chosen documents
 * into the app's cache (the native side reads plain paths) and publishes the
 * paths for Rust to collect with [takeResult].
 */
class PickerActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState != null) return
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = this@PickerActivity.intent.getStringExtra(EXTRA_MIME) ?: "*/*"
            putExtra(Intent.EXTRA_ALLOW_MULTIPLE, this@PickerActivity.intent.getBooleanExtra(EXTRA_MULTIPLE, false))
        }
        startActivityForResult(intent, REQUEST)
    }

    @Deprecated("Activity result API is unavailable without AndroidX")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val uris = mutableListOf<Uri>()
        if (resultCode == RESULT_OK && data != null) {
            val clip = data.clipData
            if (clip != null) {
                for (i in 0 until clip.itemCount) uris.add(clip.getItemAt(i).uri)
            } else {
                data.data?.let { uris.add(it) }
            }
        }
        val paths = uris.mapNotNull { copyToCache(this, it)?.absolutePath }
        synchronized(lock) { result = paths.joinToString("\n") }
        finish()
    }

    companion object {
        private const val REQUEST = 7001
        private const val EXTRA_MIME = "mime"
        private const val EXTRA_MULTIPLE = "multiple"
        private val lock = Any()
        private var result: String? = null

        /** Opens the picker for documents of the given MIME type. */
        @JvmStatic
        fun launch(context: Context, mime: String, multiple: Boolean) {
            synchronized(lock) { result = null }
            val intent = Intent(context, PickerActivity::class.java)
                .putExtra(EXTRA_MIME, mime)
                .putExtra(EXTRA_MULTIPLE, multiple)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(intent)
        }

        /**
         * Newline-separated paths once the picker closed ("" if cancelled),
         * or null while it is still open. Each result is returned once.
         */
        @JvmStatic
        fun takeResult(): String? = synchronized(lock) { result.also { result = null } }

        /**
         * Moves a received file into the shared Downloads collection under
         * "Download/Syntra", where every file manager shows it. Returns the
         * visible location, or null if publishing failed (the file stays in
         * the app folder then).
         */
        @JvmStatic
        fun publishDownload(context: Context, path: String): String? = try {
            val source = File(path)
            if (android.os.Build.VERSION.SDK_INT < 29) {
                // No MediaStore.Downloads before Android 10: leave the file
                // in the app folder rather than asking for storage access.
                throw IllegalStateException("shared Downloads needs Android 10")
            }
            val values = android.content.ContentValues().apply {
                put(android.provider.MediaStore.MediaColumns.DISPLAY_NAME, source.name)
                put(android.provider.MediaStore.MediaColumns.RELATIVE_PATH, "Download/Syntra")
                put(android.provider.MediaStore.MediaColumns.IS_PENDING, 1)
            }
            val resolver = context.contentResolver
            val collection = android.provider.MediaStore.Downloads.EXTERNAL_CONTENT_URI
            val target = resolver.insert(collection, values) ?: throw IllegalStateException("insert failed")
            resolver.openOutputStream(target)?.use { out -> source.inputStream().use { it.copyTo(out) } }
                ?: throw IllegalStateException("no output stream")
            values.clear()
            values.put(android.provider.MediaStore.MediaColumns.IS_PENDING, 0)
            resolver.update(target, values, null, null)
            source.delete()
            "Download/Syntra/" + source.name
        } catch (error: Exception) {
            null
        }

        private fun copyToCache(context: Context, uri: Uri): File? {
            return try {
            var name = "picked"
            context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
                if (it.moveToFirst()) name = it.getString(0) ?: name
            }
            val dir = File(context.cacheDir, "picked").apply { mkdirs() }
            val safe = name.replace('/', '_').ifBlank { "picked" }
            val target = File(dir, safe)
            val copied = context.contentResolver.openInputStream(uri)?.use { input ->
                target.outputStream().use { output -> input.copyTo(output) }
            }
            if (copied == null) null else target
        } catch (error: Exception) {
            null
        }
        }
    }
}

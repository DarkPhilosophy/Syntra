package io.syntra.syntra

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PixelFormat
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

/**
 * Lets a paired computer drive this phone.
 *
 * Android offers apps no way to inject input; an accessibility service is
 * the sanctioned path. It draws a pointer the computer moves, turns clicks
 * into taps and drags into gestures, the wheel into swipes, and types text
 * into the focused field. The Rust emulation backend calls the static
 * functions below; they are no-ops until the user enables the service.
 */
class SyntraAccessibilityService : AccessibilityService() {
    private val main = Handler(Looper.getMainLooper())
    private var pointer: PointerView? = null
    private var params: WindowManager.LayoutParams? = null
    @Volatile private var x = 0f
    @Volatile private var y = 0f
    private var pressedAt: Pair<Float, Float>? = null
    private var dragPath: Path? = null

    override fun onServiceConnected() {
        instance = this
        val metrics = resources.displayMetrics
        x = metrics.widthPixels / 2f
        y = metrics.heightPixels / 2f
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        main.post { hidePointer() }
        instance = null
        return super.onUnbind(intent)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}

    private fun showPointer() {
        if (pointer != null) return
        val wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
        val size = (28 * resources.displayMetrics.density).toInt()
        val lp = WindowManager.LayoutParams(
            size, size,
            WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
                WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
                WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
            PixelFormat.TRANSLUCENT,
        )
        lp.gravity = Gravity.TOP or Gravity.START
        lp.x = x.toInt(); lp.y = y.toInt()
        val view = PointerView(this)
        wm.addView(view, lp)
        pointer = view; params = lp
    }

    private fun hidePointer() {
        val view = pointer ?: return
        (getSystemService(Context.WINDOW_SERVICE) as WindowManager).removeView(view)
        pointer = null; params = null
    }

    private fun movePointer() {
        val view = pointer ?: return
        val lp = params ?: return
        lp.x = x.toInt(); lp.y = y.toInt()
        (getSystemService(Context.WINDOW_SERVICE) as WindowManager).updateViewLayout(view, lp)
    }

    private fun gesture(path: Path, duration: Long) {
        val stroke = GestureDescription.StrokeDescription(path, 0, duration.coerceAtLeast(1))
        dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), null, null)
    }

    /**
     * Moves the pointer and reports the screen edge it was pushed past:
     * 0 none, 1 left, 2 right, 3 top, 4 bottom. Pushing past the edge that
     * faces the controlling computer is how the user hands control back.
     */
    @Synchronized
    private fun advance(dx: Float, dy: Float): Int {
        val metrics = resources.displayMetrics
        val maxX = metrics.widthPixels - 1f
        val maxY = metrics.heightPixels - 1f
        val tx = x + dx
        val ty = y + dy
        val edge = when {
            tx < 0f -> 1
            tx > maxX -> 2
            ty < 0f -> 3
            ty > maxY -> 4
            else -> 0
        }
        x = tx.coerceIn(0f, maxX)
        y = ty.coerceIn(0f, maxY)
        return edge
    }

    private fun onMotion() {
        showPointer()
        movePointer()
        dragPath?.lineTo(x, y)
    }

    private fun onButton(pressed: Boolean) {
        showPointer()
        if (pressed) {
            pressedAt = x to y
            dragPath = Path().apply { moveTo(x, y) }
            return
        }
        val start = pressedAt ?: return
        val path = dragPath ?: Path().apply { moveTo(start.first, start.second) }
        pressedAt = null; dragPath = null
        val moved = kotlin.math.abs(x - start.first) + kotlin.math.abs(y - start.second) > 12f
        if (moved) {
            // Press, move and release replayed as one drag.
            gesture(path, 350)
        } else {
            gesture(Path().apply { moveTo(start.first, start.second) }, 40)
        }
    }

    private fun onScroll(detents: Float) {
        showPointer()
        // One wheel detent swipes a fifth of the screen, opposite to the
        // wheel like a real scroll: wheel down moves content up.
        val distance = detents * resources.displayMetrics.heightPixels / 5f
        val metrics = resources.displayMetrics
        val fromY = y.coerceIn(metrics.heightPixels * 0.2f, metrics.heightPixels * 0.8f)
        val toY = (fromY - distance).coerceIn(1f, metrics.heightPixels - 1f)
        gesture(Path().apply { moveTo(x, fromY); lineTo(x, toY) }, 120)
    }

    private fun focusedField(): AccessibilityNodeInfo? =
        rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)

    private fun onText(text: String) {
        val node = focusedField() ?: return
        val current = node.text?.toString() ?: ""
        val args = Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, current + text)
        }
        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
    }

    private fun onBackspace() {
        val node = focusedField() ?: return
        val current = node.text?.toString() ?: return
        if (current.isEmpty()) return
        val args = Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, current.dropLast(1))
        }
        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
    }

    private class PointerView(context: Context) : View(context) {
        private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = Color.WHITE }
        private val edge = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = Color.BLACK; style = Paint.Style.STROKE; strokeWidth = 3f
        }
        override fun onDraw(canvas: Canvas) {
            val w = width.toFloat(); val h = height.toFloat()
            val arrow = Path().apply {
                moveTo(2f, 2f); lineTo(2f, h * 0.85f); lineTo(w * 0.3f, h * 0.62f)
                lineTo(w * 0.5f, h * 0.98f); lineTo(w * 0.65f, h * 0.9f)
                lineTo(w * 0.46f, h * 0.55f); lineTo(w * 0.8f, h * 0.55f); close()
            }
            canvas.drawPath(arrow, fill)
            canvas.drawPath(arrow, edge)
        }
    }

    companion object {
        @Volatile private var instance: SyntraAccessibilityService? = null

        private fun run(block: SyntraAccessibilityService.() -> Unit) {
            val service = instance ?: return
            service.main.post { service.block() }
        }

        @JvmStatic fun isEnabled(): Boolean = instance != null
        @JvmStatic fun motion(dx: Float, dy: Float): Int {
            val service = instance ?: return 0
            val edge = service.advance(dx, dy)
            service.main.post { service.onMotion() }
            return edge
        }
        @JvmStatic fun button(pressed: Boolean) = run { onButton(pressed) }
        @JvmStatic fun scroll(detents: Float) = run { onScroll(detents) }
        @JvmStatic fun text(text: String) = run { onText(text) }
        @JvmStatic fun backspace() = run { onBackspace() }
        @JvmStatic fun back() = run { performGlobalAction(GLOBAL_ACTION_BACK) }
        @JvmStatic fun home() = run { performGlobalAction(GLOBAL_ACTION_HOME) }
        @JvmStatic fun leave() = run { hidePointer() }
    }
}

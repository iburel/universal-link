package dev.universallink.app

import android.app.Activity
import android.os.Bundle
import android.widget.ScrollView
import android.widget.TextView

class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val text = TextView(this).apply {
            // Top pad clears the status/action bar (edge-to-edge on targetSdk 36).
            setPadding(48, 520, 48, 48)
            textSize = 15f
            setTextIsSelectable(true)
            text = "UniversalLink — brick 1\nstarting embedded core…"
        }
        setContentView(ScrollView(this).apply { addView(text) })

        Thread {
            val dataDir = filesDir.absolutePath
            val nativeSummary = try {
                NativeCore.nativeStart(dataDir)
            } catch (e: Throwable) {
                "native start threw: ${e.javaClass.simpleName}: ${e.message}"
            }
            val helloLine = UdsHello.run(dataDir)
            val full = buildString {
                append("UniversalLink — brick 1\n\n")
                append("[native / embedded core]\n")
                append(nativeSummary)
                append("\n\n[kotlin ↔ uds hello]\n")
                append(helloLine)
            }
            android.util.Log.i("ULApp", "brick1 result:\n$full")
            runOnUiThread { text.text = full }
        }.start()
    }
}

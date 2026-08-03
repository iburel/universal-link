package org.universallink.mobile

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.util.Log
import android.util.Size
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.TextView
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.MultiFormatReader
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The camera, pointed at another device's screen. Opened by [ScanBridge.startScan]
 * and reporting its outcome back through it — see `gui-mobile/src/scan.rs` for the
 * flow this is one step of, and [ScanBridge] for why the decoding is done here
 * rather than handed to Google Play Services.
 *
 * The contract with the Rust side is that EXACTLY ONE outcome is reported per
 * scan, whatever happens to this window: a code, a refused permission, a camera
 * that would not open, or the user backing out. [onDestroy] is what closes that
 * off — every other exit path goes through [finishWith] first, and [reported]
 * makes the last word the first one spoken. Without it a command on the other side
 * would hang until its backstop, five minutes later, with the button disarmed.
 */
class ScanActivity : AppCompatActivity() {
    /** What a code may start with to be one of ours (from the Core, via Rust). */
    private var tags: List<String> = emptyList()

    /** Whether an outcome has been reported: at most one crosses the seam. */
    private val reported = AtomicBoolean(false)

    /** Whether the hint has already been rewritten for a foreign code. */
    private val sawForeign = AtomicBoolean(false)

    private lateinit var preview: PreviewView
    private lateinit var hint: TextView

    /**
     * One thread for the decoding, and only one: `STRATEGY_KEEP_ONLY_LATEST`
     * drops the frames that arrive while it is busy, which is exactly the right
     * behaviour for a scanner — the next frame is always more useful than a queue
     * of stale ones.
     */
    private val decoder: ExecutorService = Executors.newSingleThreadExecutor()

    /** Reused across frames: `decodeWithState` is built for being called in a loop. */
    private val reader = MultiFormatReader().apply {
        // QR only. Left open, a scanner reads the barcode on the back of the
        // laptop it is pointed at.
        setHints(
            mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE)),
        )
    }

    private val askCamera = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (granted) {
            startCamera()
        } else {
            // Not an error to retry: the user said no, and the pairing code can
            // still be pasted. The frontend has a sentence for this.
            finishWith(ScanBridge.failure("denied"))
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        tags = intent.getStringExtra(ScanBridge.EXTRA_TAG).orEmpty()
            .split(',').filter { it.isNotEmpty() }
        setContentView(R.layout.activity_scan)
        preview = findViewById(R.id.scan_preview)
        hint = findViewById(R.id.scan_hint)
        findViewById<Button>(R.id.scan_cancel).setOnClickListener {
            finishWith(ScanBridge.failure("cancelled"))
        }
        // Framing a code takes both hands and no taps: the screen must not go off
        // while it is being done.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        insetBar()

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA)
            == PackageManager.PERMISSION_GRANTED
        ) {
            startCamera()
        } else {
            askCamera.launch(Manifest.permission.CAMERA)
        }
    }

    /**
     * Keeps the bar clear of the gesture bar and the cutout. Observed at the DECOR
     * view for the reason MainActivity documents at length: AppCompat's decor
     * consumes the system-window insets before anything below it is asked, so a
     * listener on the bar itself never fires.
     */
    private fun insetBar() {
        val bar = findViewById<View>(R.id.scan_bar)
        ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { v, insets ->
            val safe = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
            )
            bar.setPadding(safe.left, bar.paddingTop, safe.right, safe.bottom)
            ViewCompat.onApplyWindowInsets(v, insets)
        }
    }

    private fun startCamera() {
        val future = ProcessCameraProvider.getInstance(this)
        future.addListener({
            val provider = try {
                future.get()
            } catch (t: Throwable) {
                Log.e(TAG, "no camera provider", t)
                finishWith(ScanBridge.failure("no_camera"))
                return@addListener
            }
            bind(provider)
        }, ContextCompat.getMainExecutor(this))
    }

    private fun bind(provider: ProcessCameraProvider) {
        val view = Preview.Builder().build()
        view.setSurfaceProvider(preview.surfaceProvider)
        val analysis = ImageAnalysis.Builder()
            // The default analysis resolution is 640×480, and the code will be
            // somewhere IN the frame rather than filling it: a pairing code is a
            // 41-module symbol, and across a third of a 480-line frame that is
            // under four pixels per module. A noiseless frame still decodes at
            // two, so 720p is margin against blur and camera noise rather than a
            // floor — with whatever the camera has nearest as the fallback.
            .setResolutionSelector(
                ResolutionSelector.Builder()
                    .setResolutionStrategy(
                        ResolutionStrategy(
                            Size(1280, 720),
                            ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER,
                        ),
                    )
                    .build(),
            )
            .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
            .build()
        analysis.setAnalyzer(decoder) { image -> analyze(image) }

        // The rear camera is the one that can be pointed at another screen; a
        // device with only a front one (a tablet, a TV) still gets to scan.
        for (selector in listOf(
            CameraSelector.DEFAULT_BACK_CAMERA,
            CameraSelector.DEFAULT_FRONT_CAMERA,
        )) {
            try {
                provider.unbindAll()
                provider.bindToLifecycle(this, selector, view, analysis)
                return
            } catch (t: Throwable) {
                Log.w(TAG, "could not bind $selector", t)
            }
        }
        finishWith(ScanBridge.failure("no_camera"))
    }

    /**
     * One frame. The luminance plane IS the image as far as a QR code is
     * concerned, so nothing is converted: the Y plane is handed to zxing as it
     * comes off the camera, with its own row stride as the width (the rows are
     * padded, and reading them as if they were `image.width` wide would shear the
     * picture).
     *
     * Rotation is not corrected either — zxing finds the three finder patterns
     * wherever they are, so a code read sideways decodes the same.
     */
    private fun analyze(image: ImageProxy) {
        try {
            if (reported.get()) return
            val plane = image.planes[0]
            val buffer = plane.buffer
            val luminance = ByteArray(buffer.remaining())
            buffer.get(luminance)
            val stride = plane.rowStride
            val source = PlanarYUVLuminanceSource(
                luminance,
                stride,
                image.height,
                0,
                0,
                minOf(image.width, stride),
                image.height,
                false,
            )
            val text = try {
                reader.decodeWithState(BinaryBitmap(HybridBinarizer(source))).text
            } catch (e: ReaderException) {
                // No code in this frame, or one too damaged to read. The next
                // frame is the answer, not an error.
                null
            }
            if (text == null) return
            if (tags.isNotEmpty() && tags.none { text.startsWith(it) }) {
                // Somebody else's QR code in the camera's view. Keep looking, and
                // say so once: a scanner that silently ignores what it is pointed
                // at looks broken.
                if (sawForeign.compareAndSet(false, true)) {
                    runOnUiThread { hint.setText(R.string.scan_foreign) }
                }
                return
            }
            runOnUiThread { finishWith(ScanBridge.code(text)) }
        } catch (t: Throwable) {
            // A frame this thread cannot handle must not take the scan down: the
            // camera keeps delivering, and the next one may well decode.
            Log.w(TAG, "a frame could not be read", t)
        } finally {
            image.close()
        }
    }

    /** Reports an outcome, at most once, whoever gets here first. */
    private fun report(json: String) {
        if (!reported.compareAndSet(false, true)) return
        ScanBridge.report(json)
    }

    private fun finishWith(json: String) {
        report(json)
        finish()
    }

    override fun onDestroy() {
        // Back pressed, or the system reclaiming the window: whatever the Rust
        // side is waiting for, it gets an answer. A configuration change is NOT
        // one of those — the manifest keeps rotation from recreating this
        // activity, and this guard covers whatever it does not.
        if (!isChangingConfigurations) {
            report(ScanBridge.failure("cancelled"))
        }
        decoder.shutdown()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "ULCore"
    }
}

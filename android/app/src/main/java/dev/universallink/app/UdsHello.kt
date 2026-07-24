package dev.universallink.app

import android.net.LocalSocket
import android.net.LocalSocketAddress
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.InputStream

/**
 * Proves the 4th-client seam: connects to the embedded Core's UDS as a
 * component and performs the core-api `hello` handshake with the file token,
 * using the same LSP-style framing (Content-Length + JSON) as the desktop
 * components. No new core API — this is exactly what the Kotlin app will speak.
 */
object UdsHello {
    fun run(dataDir: String): String {
        val sockPath = "$dataDir/core.sock"
        val tokenFile = File("$dataDir/ipc-token")

        // The Core boots on its own thread; give the socket + token a moment.
        var waited = 0
        while ((!File(sockPath).exists() || !tokenFile.exists()) && waited < 5000) {
            Thread.sleep(100)
            waited += 100
        }
        if (!tokenFile.exists()) return "no ipc-token after ${waited}ms"
        val token = tokenFile.readText().trim()

        return try {
            val socket = LocalSocket()
            socket.connect(
                LocalSocketAddress(sockPath, LocalSocketAddress.Namespace.FILESYSTEM)
            )
            socket.use {
                val hello = JSONObject()
                    .put("jsonrpc", "2.0")
                    .put("id", 1)
                    .put("method", "hello")
                    .put(
                        "params",
                        JSONObject()
                            .put("name", "android-smoke")
                            .put("version", "0.1.0")
                            .put("role", "custom")
                            .put("scopes", JSONArray())
                            .put("token", token)
                    )
                    .toString()

                val body = hello.toByteArray(Charsets.UTF_8)
                val out = socket.outputStream
                out.write("Content-Length: ${body.size}\r\n\r\n".toByteArray(Charsets.US_ASCII))
                out.write(body)
                out.flush()

                val frame = readFrame(socket.inputStream) ?: return "no reply frame"
                val reply = JSONObject(frame)
                val result = reply.optJSONObject("result")
                if (result != null) {
                    "OK status=${result.optString("status")} " +
                        "api_version=${result.optInt("api_version")}"
                } else {
                    "error reply: $frame"
                }
            }
        } catch (e: Throwable) {
            "hello failed: ${e.javaClass.simpleName}: ${e.message}"
        }
    }

    private fun readFrame(inp: InputStream): String? {
        var contentLength = -1
        while (true) {
            val line = readHeaderLine(inp) ?: return null
            if (line.isEmpty()) break
            val idx = line.indexOf(':')
            if (idx > 0 && line.substring(0, idx).trim().equals("content-length", true)) {
                contentLength = line.substring(idx + 1).trim().toIntOrNull() ?: return null
            }
        }
        if (contentLength < 0) return null
        val buf = ByteArray(contentLength)
        var read = 0
        while (read < contentLength) {
            val n = inp.read(buf, read, contentLength - read)
            if (n < 0) return null
            read += n
        }
        return String(buf, Charsets.UTF_8)
    }

    private fun readHeaderLine(inp: InputStream): String? {
        val sb = StringBuilder()
        while (true) {
            val b = inp.read()
            if (b < 0) return if (sb.isEmpty()) null else sb.toString()
            if (b == '\n'.code) {
                if (sb.isNotEmpty() && sb.last() == '\r') sb.deleteCharAt(sb.length - 1)
                return sb.toString()
            }
            sb.append(b.toChar())
        }
    }
}

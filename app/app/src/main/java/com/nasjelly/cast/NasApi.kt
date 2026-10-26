package com.nasjelly.cast

import org.json.JSONArray
import org.json.JSONObject
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

data class Item(val id: String, val name: String, val isFolder: Boolean)

data class SubTrack(val trackId: String, val title: String, val language: String?, val url: String)

/** What to hand the Chromecast: a URL it can fetch itself, and its MIME type. */
data class CastTarget(val url: String, val mime: String)

/** Client for the NasJelly (Jellyfin-style) endpoints. Blocking; call off the main thread. */
open class NasApi(val base: String, var token: String = "") {

    fun login(user: String, password: String) {
        val body = JSONObject().put("Username", user).put("Pw", password).toString()
        val resp = request("POST", "/Users/AuthenticateByName", body)
        token = JSONObject(resp).getString("AccessToken")
    }

    open fun views(): List<Item> = items(request("GET", "/UserViews"))

    open fun children(parentId: String): List<Item> =
        items(request("GET", "/Items?ParentId=$parentId&SortBy=SortName"))

    /**
     * Asks the server how to play [id] on a Chromecast. It either direct-plays the file, or
     * remuxes to HLS with ffmpeg (audio converted to AAC if needed). Video is never re-encoded.
     */
    open fun castTarget(id: String): CastTarget {
        val body = JSONObject().put("DeviceProfile", JSONObject(CHROMECAST_PROFILE)).toString()
        val resp = JSONObject(request("POST", "/Items/$id/PlaybackInfo", body))
        val src = resp.getJSONArray("MediaSources").getJSONObject(0)

        val transcodingUrl = src.optString("TranscodingUrl")
        if (transcodingUrl.isNotEmpty()) {
            return CastTarget(base + transcodingUrl, "application/x-mpegURL")
        }
        if (src.optBoolean("SupportsDirectPlay")) {
            val mime = if (src.optString("Container") == "webm") "video/webm" else "video/mp4"
            return CastTarget("$base/Videos/$id/stream?api_key=$token&Static=true", mime)
        }
        throw IOException("Video codec not supported by Chromecast (server can't re-encode video)")
    }

    /** Text subtitles the server can serve as WebVTT: tracks inside the file and external .srt files. */
    open fun subtitleTracks(id: String): List<SubTrack> {
        val arr = JSONObject(request("GET", "/Videos/$id/subtitletracks")).getJSONArray("Tracks")
        return (0 until arr.length()).map {
            val o = arr.getJSONObject(it)
            SubTrack(
                trackId = o.getString("id"),
                title = o.getString("title"),
                language = o.optString("language").takeIf { l -> l.isNotEmpty() && l != "null" },
                url = "$base/Videos/$id/subtitle.vtt?track=${o.getString("id")}&api_key=$token",
            )
        }
    }

    private fun items(json: String): List<Item> {
        val arr: JSONArray = JSONObject(json).getJSONArray("Items")
        return (0 until arr.length()).map {
            val o = arr.getJSONObject(it)
            Item(o.getString("Id"), o.getString("Name"), o.optBoolean("IsFolder"))
        }
    }

    private fun request(method: String, path: String, body: String? = null): String {
        val conn = URL(base + path).openConnection() as HttpURLConnection
        try {
            conn.requestMethod = method
            conn.connectTimeout = 10_000
            conn.readTimeout = 90_000 // first PlaybackInfo on a file runs ffprobe
            conn.setRequestProperty("X-Emby-Authorization", AUTH_HEADER)
            if (token.isNotEmpty()) conn.setRequestProperty("X-Emby-Token", token)
            if (body != null) {
                conn.doOutput = true
                conn.setRequestProperty("Content-Type", "application/json")
                conn.outputStream.use { it.write(body.toByteArray()) }
            }
            if (conn.responseCode !in 200..299) {
                throw IOException("Server returned HTTP ${conn.responseCode}")
            }
            return conn.inputStream.bufferedReader().use { it.readText() }
        } finally {
            conn.disconnect()
        }
    }

    companion object {
        fun normalize(server: String): String {
            val s = server.trim().trimEnd('/')
            return if (s.startsWith("http://") || s.startsWith("https://")) s else "http://$s"
        }

        private const val AUTH_HEADER =
            "MediaBrowser Client=\"NasJelly Cast\", Device=\"Android\", DeviceId=\"nasjelly-cast\", Version=\"1.0\""

        // Conservative Chromecast capabilities: H.264 MP4 (or VP9 WebM) plays as-is;
        // anything else is remuxed to HLS/TS with AAC stereo audio.
        private const val CHROMECAST_PROFILE = """{
            "DirectPlayProfiles": [
              {"Container":"mp4,m4v","Type":"Video","VideoCodec":"h264","AudioCodec":"aac,mp3"},
              {"Container":"webm","Type":"Video","VideoCodec":"vp8,vp9","AudioCodec":"opus,vorbis"}
            ],
            "TranscodingProfiles": [
              {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264",
               "AudioCodec":"aac,mp3","MaxAudioChannels":"2"}
            ]
        }"""
    }
}

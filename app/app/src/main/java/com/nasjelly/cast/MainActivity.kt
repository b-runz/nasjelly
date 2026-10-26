package com.nasjelly.cast

import android.graphics.Color
import android.os.Bundle
import android.view.Menu
import android.view.MenuItem
import android.view.View
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.ListView
import android.widget.TextView
import androidx.activity.OnBackPressedCallback
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.core.widget.doAfterTextChanged
import com.google.android.gms.cast.MediaInfo
import com.google.android.gms.cast.MediaLoadRequestData
import com.google.android.gms.cast.MediaMetadata
import com.google.android.gms.cast.MediaTrack
import com.google.android.gms.cast.TextTrackStyle
import com.google.android.gms.cast.framework.CastButtonFactory
import com.google.android.gms.cast.framework.CastContext
import java.util.concurrent.Executors

class MainActivity : AppCompatActivity() {
    private val io = Executors.newSingleThreadExecutor()
    private val prefs by lazy { getSharedPreferences("nasjelly", MODE_PRIVATE) }

    private var api: NasApi? = null
    private val trail = ArrayList<Item>() // folders we've descended into
    private var all: List<Item> = emptyList()
    private val shown = ArrayList<Item>()
    private lateinit var adapter: ArrayAdapter<String>
    private var castContext: CastContext? = null

    private lateinit var loginPanel: View
    private lateinit var browsePanel: View
    private lateinit var status: TextView
    private lateinit var path: TextView
    private lateinit var filter: EditText
    private lateinit var tvPreview: View
    private lateinit var tvTitle: TextView
    private lateinit var tvSub: TextView

    // demo mode: a fake "TV" stands in for the Chromecast
    private val isDemo get() = api is DemoApi
    private var demoTitle = ""
    private var demoTracks: List<SubTrack> = emptyList()
    private var demoActive = 0 // 0 = off, else 1-based index into demoTracks
    private var demoPaused = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        loginPanel = findViewById(R.id.login_panel)
        browsePanel = findViewById(R.id.browse_panel)
        status = findViewById(R.id.status)
        path = findViewById(R.id.path)
        filter = findViewById(R.id.filter)
        tvPreview = findViewById(R.id.tv_preview)
        tvTitle = findViewById(R.id.tv_title)
        tvSub = findViewById(R.id.tv_sub)

        castContext = try {
            CastContext.getSharedInstance(this)
        } catch (e: Exception) {
            status.text = "Cast unavailable: ${e.message}"
            null
        }

        val server = findViewById<EditText>(R.id.server)
        val user = findViewById<EditText>(R.id.username)
        val pass = findViewById<EditText>(R.id.password)
        server.setText(prefs.getString("server", ""))
        user.setText(prefs.getString("user", ""))
        pass.setText(prefs.getString("pass", ""))
        findViewById<Button>(R.id.connect).setOnClickListener {
            connect(server.text.toString(), user.text.toString(), pass.text.toString())
        }

        adapter = ArrayAdapter(this, android.R.layout.simple_list_item_1, ArrayList<String>())
        val list = findViewById<ListView>(R.id.list)
        list.adapter = adapter
        list.setOnItemClickListener { _, _, pos, _ ->
            val item = shown[pos]
            if (item.isFolder) {
                trail.add(item)
                load()
            } else {
                cast(item)
            }
        }
        filter.doAfterTextChanged { applyFilter() }

        findViewById<Button>(R.id.find).setOnClickListener { findServers(server) }
        findViewById<Button>(R.id.demo).setOnClickListener {
            api = DemoApi()
            trail.clear()
            loginPanel.visibility = View.GONE
            browsePanel.visibility = View.VISIBLE
            load()
        }
        findViewById<Button>(R.id.play_pause).setOnClickListener {
            if (isDemo) demoTogglePause() else remoteClient()?.togglePlayback()
        }
        findViewById<Button>(R.id.stop).setOnClickListener {
            if (isDemo) demoStop() else remoteClient()?.stop()
        }

        findViewById<Button>(R.id.subtitles).setOnClickListener { pickSubtitleTrack() }
        findViewById<Button>(R.id.sub_size).setOnClickListener { pickSubtitleSize() }

        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                if (browsePanel.visibility == View.VISIBLE && trail.isNotEmpty()) {
                    trail.removeAt(trail.size - 1)
                    load()
                } else {
                    isEnabled = false
                    onBackPressedDispatcher.onBackPressed()
                }
            }
        })

        // Reconnect automatically with the saved login.
        if (server.text.isNotBlank() && user.text.isNotBlank()) {
            connect(server.text.toString(), user.text.toString(), pass.text.toString())
        } else if (server.text.isBlank()) {
            findServers(server)
        }
    }

    /** Fills [field] with a server found by broadcast; asks which one if several answer. */
    private fun findServers(field: EditText) {
        status.text = "Looking for servers..."
        io.execute {
            val found = try {
                Discovery.find(this)
            } catch (e: Exception) {
                runOnUiThread { status.text = "Search failed: ${e.message}" }
                return@execute
            }
            runOnUiThread {
                when (found.size) {
                    0 -> status.text = "No server found. Is this phone on the same Wi-Fi? Enter the address by hand."
                    1 -> {
                        field.setText(found[0].address)
                        status.text = "Found ${found[0].name}"
                    }
                    else -> AlertDialog.Builder(this)
                        .setTitle("Pick a server")
                        .setItems(found.map { "${it.name}  (${it.address})" }.toTypedArray()) { _, which ->
                            field.setText(found[which].address)
                            status.text = "Selected ${found[which].name}"
                        }
                        .show()
                }
            }
        }
    }

    override fun onCreateOptionsMenu(menu: Menu): Boolean {
        menuInflater.inflate(R.menu.main, menu)
        if (castContext != null) {
            CastButtonFactory.setUpMediaRouteButton(applicationContext, menu, R.id.media_route_menu_item)
        }
        return true
    }

    override fun onOptionsItemSelected(item: MenuItem): Boolean {
        if (item.itemId == R.id.menu_logout) {
            demoStop()
            api = null
            trail.clear()
            browsePanel.visibility = View.GONE
            loginPanel.visibility = View.VISIBLE
            return true
        }
        return super.onOptionsItemSelected(item)
    }

    private fun connect(server: String, user: String, pass: String) {
        val client = NasApi(NasApi.normalize(server))
        status.text = "Connecting..."
        io.execute {
            try {
                client.login(user, pass)
                runOnUiThread {
                    prefs.edit().putString("server", server).putString("user", user)
                        .putString("pass", pass).apply()
                    api = client
                    trail.clear()
                    loginPanel.visibility = View.GONE
                    browsePanel.visibility = View.VISIBLE
                    load()
                }
            } catch (e: Exception) {
                runOnUiThread { status.text = "Connect failed: ${e.message}" }
            }
        }
    }

    /** Lists the folder at the top of [trail], or the server's root folders if empty. */
    private fun load() {
        val client = api ?: return
        val parent = trail.lastOrNull()
        path.text = "/" + trail.joinToString("/") { it.name }
        status.text = "Loading..."
        io.execute {
            try {
                val items = if (parent == null) client.views() else client.children(parent.id)
                runOnUiThread {
                    all = items
                    filter.setText("")
                    applyFilter()
                    status.text = "${items.size} items"
                }
            } catch (e: Exception) {
                runOnUiThread { status.text = "Load failed: ${e.message}" }
            }
        }
    }

    private fun applyFilter() {
        val q = filter.text.toString().trim().lowercase()
        shown.clear()
        shown.addAll(all.filter { q.isEmpty() || it.name.lowercase().contains(q) })
        adapter.clear()
        adapter.addAll(shown.map { if (it.isFolder) "[ ${it.name} ]" else it.name })
    }

    private fun cast(item: Item) {
        val client = api ?: return
        if (client is DemoApi) {
            demoCast(item, client)
            return
        }
        val session = castContext?.sessionManager?.currentCastSession
        if (session == null || !session.isConnected) {
            status.text = "Pick a Chromecast with the cast button first"
            return
        }
        status.text = "Preparing ${item.name}..."
        io.execute {
            try {
                val target = client.castTarget(item.id)
                // subtitles are optional; a failure here shouldn't stop the video
                val subs = try { client.subtitleTracks(item.id) } catch (e: Exception) { emptyList() }
                val meta = MediaMetadata(MediaMetadata.MEDIA_TYPE_MOVIE)
                meta.putString(MediaMetadata.KEY_TITLE, item.name)
                // all tracks are loaded up front, off by default; the Subtitles button switches live
                val tracks = subs.mapIndexed { i, s ->
                    MediaTrack.Builder(i + 1L, MediaTrack.TYPE_TEXT)
                        .setSubtype(MediaTrack.SUBTYPE_SUBTITLES)
                        .setContentId(s.url)
                        .setContentType("text/vtt")
                        .setName(s.title)
                        .setLanguage(s.language)
                        .build()
                }
                val info = MediaInfo.Builder(target.url)
                    .setStreamType(MediaInfo.STREAM_TYPE_BUFFERED)
                    .setContentType(target.mime)
                    .setMetadata(meta)
                    .setMediaTracks(tracks)
                    .setTextTrackStyle(subtitleStyle())
                    .build()
                runOnUiThread {
                    session.remoteMediaClient?.load(
                        MediaLoadRequestData.Builder().setMediaInfo(info).setAutoplay(true).build()
                    )
                    status.text = "Casting ${item.name} (${subs.size} subtitle tracks)"
                }
            } catch (e: Exception) {
                runOnUiThread { status.text = "Can't cast: ${e.message}" }
            }
        }
    }

    private val sizeLabels = arrayOf("Small", "Normal", "Large", "Extra large")
    private val sizeScales = floatArrayOf(0.7f, 1.0f, 1.4f, 1.8f)

    /** White text with a black outline, no background box; scaled by the saved size. */
    private fun subtitleStyle(): TextTrackStyle = TextTrackStyle().apply {
        foregroundColor = Color.WHITE
        backgroundColor = Color.TRANSPARENT
        windowColor = Color.TRANSPARENT
        edgeType = TextTrackStyle.EDGE_TYPE_OUTLINE
        edgeColor = Color.BLACK
        fontGenericFamily = TextTrackStyle.FONT_FAMILY_SANS_SERIF
        fontScale = sizeScales[prefs.getInt("subSize", 1)]
    }

    private fun remoteClient() = castContext?.sessionManager?.currentCastSession?.remoteMediaClient

    private fun demoCast(item: Item, client: DemoApi) {
        demoTitle = item.name
        demoTracks = client.subtitleTracks(item.id)
        demoActive = 0
        demoPaused = false
        tvPreview.visibility = View.VISIBLE
        status.text = "Casting ${item.name} (${demoTracks.size} subtitle tracks) [demo]"
        updateDemoPreview()
    }

    private fun demoTogglePause() {
        if (tvPreview.visibility != View.VISIBLE) return
        demoPaused = !demoPaused
        status.text = if (demoPaused) "Paused [demo]" else "Casting $demoTitle [demo]"
        updateDemoPreview()
    }

    private fun demoStop() {
        tvPreview.visibility = View.GONE
        demoTracks = emptyList()
        if (isDemo) status.text = "Stopped [demo]"
    }

    /** Approximates the receiver: white text, dark outline, scaled by the chosen size. */
    private fun updateDemoPreview() {
        tvTitle.text = (if (demoPaused) "⏸ " else "▶ ") + demoTitle
        val track = demoTracks.getOrNull(demoActive - 1)
        if (track == null) {
            tvSub.text = ""
            return
        }
        tvSub.text = when (track.language) {
            "dan" -> "Sådan ser undertekster ud.\nBlåbærgrød med fløde."
            else -> "This is how the subtitles look.\nA second line, a little longer than the first."
        }
        tvSub.textSize = 16f * sizeScales[prefs.getInt("subSize", 1)]
        tvSub.setShadowLayer(5f, 0f, 0f, Color.BLACK)
    }

    private fun pickSubtitleTrack() {
        if (isDemo) {
            if (tvPreview.visibility != View.VISIBLE) {
                status.text = "Nothing is casting"
                return
            }
            if (demoTracks.isEmpty()) {
                status.text = "No text subtitle tracks for this file"
                return
            }
            val names = (listOf("Off") + demoTracks.map { it.title }).toTypedArray()
            AlertDialog.Builder(this)
                .setTitle("Subtitles")
                .setSingleChoiceItems(names, demoActive) { dialog, which ->
                    demoActive = which
                    updateDemoPreview()
                    dialog.dismiss()
                }
                .show()
            return
        }
        val client = remoteClient()
        val tracks = client?.mediaInfo?.mediaTracks?.filter { it.type == MediaTrack.TYPE_TEXT }
        if (client == null || tracks == null) {
            status.text = "Nothing is casting"
            return
        }
        if (tracks.isEmpty()) {
            status.text = "No text subtitle tracks for this file"
            return
        }
        val names = (listOf("Off") + tracks.map { it.name ?: "Track ${it.id}" }).toTypedArray()
        val active = client.mediaStatus?.activeTrackIds?.firstOrNull()
        val checked = tracks.indexOfFirst { it.id == active } + 1
        AlertDialog.Builder(this)
            .setTitle("Subtitles")
            .setSingleChoiceItems(names, checked) { dialog, which ->
                client.setActiveMediaTracks(if (which == 0) longArrayOf() else longArrayOf(tracks[which - 1].id))
                dialog.dismiss()
            }
            .show()
    }

    private fun pickSubtitleSize() {
        AlertDialog.Builder(this)
            .setTitle("Subtitle size")
            .setSingleChoiceItems(sizeLabels, prefs.getInt("subSize", 1)) { dialog, which ->
                prefs.edit().putInt("subSize", which).apply()
                remoteClient()?.setTextTrackStyle(subtitleStyle()) // applies to the running video
                if (isDemo) updateDemoPreview()
                dialog.dismiss()
            }
            .show()
    }
}

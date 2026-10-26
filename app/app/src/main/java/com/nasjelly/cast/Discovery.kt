package com.nasjelly.cast

import android.content.Context
import android.net.wifi.WifiManager
import org.json.JSONObject
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.SocketTimeoutException

data class FoundServer(val name: String, val address: String)

/** Jellyfin's LAN discovery: broadcast "Who is JellyfinServer?" on UDP 7359, servers reply with JSON. */
object Discovery {
    private const val PORT = 7359

    /** Blocking; call off the main thread. Collects replies for [waitMs]. */
    fun find(context: Context, waitMs: Int = 2500): List<FoundServer> {
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        val lock = wifi?.createMulticastLock("nasjelly-discovery")?.apply { acquire() }
        val found = LinkedHashMap<String, FoundServer>()
        try {
            DatagramSocket().use { sock ->
                sock.broadcast = true
                val query = "Who is JellyfinServer?".toByteArray()
                sock.send(DatagramPacket(query, query.size, InetAddress.getByName("255.255.255.255"), PORT))

                val deadline = System.currentTimeMillis() + waitMs
                val buf = ByteArray(2048)
                while (true) {
                    val left = (deadline - System.currentTimeMillis()).toInt()
                    if (left <= 0) break
                    sock.soTimeout = left
                    val pkt = DatagramPacket(buf, buf.size)
                    try {
                        sock.receive(pkt)
                    } catch (e: SocketTimeoutException) {
                        break
                    }
                    try {
                        val o = JSONObject(String(pkt.data, 0, pkt.length))
                        val address = o.getString("Address")
                        found[address] = FoundServer(o.optString("Name", address), address)
                    } catch (e: Exception) {
                        // not a Jellyfin reply; ignore
                    }
                }
            }
        } finally {
            lock?.release()
        }
        return found.values.toList()
    }
}

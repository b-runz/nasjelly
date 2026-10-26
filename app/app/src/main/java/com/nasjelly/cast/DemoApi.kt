package com.nasjelly.cast

/** Fake NAS for trying the UI with no server or Chromecast. */
class DemoApi : NasApi("demo://nas", "demo") {
    private val tree = mapOf(
        "movies" to listOf(
            Item("action", "Action", true),
            Item("bbb", "Big Buck Bunny (2008).mp4", false),
            Item("sintel", "Sintel (2010).mkv", false),
            Item("tears", "Tears of Steel (2012).mkv", false),
            Item("nosubs", "Home Video 2019.mp4", false),
        ),
        "action" to listOf(
            Item("heat", "Heat (1995).mkv", false),
            Item("matrix", "The Matrix (1999).mkv", false),
        ),
        "downloads" to listOf(
            Item("dl1", "some.show.s01e01.1080p.mkv", false),
            Item("dl2", "some.show.s01e02.1080p.mkv", false),
        ),
    )

    override fun views() = listOf(Item("movies", "Movies", true), Item("downloads", "Downloads", true))

    override fun children(parentId: String) = tree[parentId].orEmpty()

    override fun castTarget(id: String) = CastTarget("demo://$id", "video/mp4")

    override fun subtitleTracks(id: String): List<SubTrack> = when (id) {
        "nosubs" -> emptyList()
        "bbb" -> listOf(SubTrack("x0", "English (file)", "eng", ""))
        else -> listOf(
            SubTrack("e2", "ENG subrip", "eng", ""),
            SubTrack("e3", "DAN subrip (forced)", "dan", ""),
            SubTrack("x0", "Movie.en (file)", "eng", ""),
        )
    }
}

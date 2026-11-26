package com.chatter.client

import org.junit.Assert.assertEquals
import org.junit.Test

class UnifiedPushConsentTest {
    @Test
    fun installedDistributorIsOfferedNotPicked() {
        assertEquals(listOf("io.heckel.ntfy"), pickableDistributors(null, listOf("io.heckel.ntfy"), emptySet()))
    }

    @Test
    fun nothingIsOfferedOnceTheUserChose() {
        assertEquals(emptyList<String>(), pickableDistributors("io.heckel.ntfy", listOf("io.heckel.ntfy", "org.other"), emptySet()))
    }

    @Test
    fun declinedDistributorsAreNotOfferedAgainButNewOnesAre() {
        val declined = setOf("io.heckel.ntfy")
        assertEquals(emptyList<String>(), pickableDistributors(null, listOf("io.heckel.ntfy"), declined))
        assertEquals(listOf("org.other"), pickableDistributors(null, listOf("io.heckel.ntfy", "org.other"), declined))
    }
}
